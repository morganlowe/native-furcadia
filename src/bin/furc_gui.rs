//! Native cross-platform GUI client for Furcadia using `eframe` / `egui`.
//!
//! # Architecture Overview
//! - **UI Thread ([`FurcadiaApp`])**: Renders the `egui` interface (login, character
//!   select, isometric map viewport, chat/whisper tabs, Pounce panel, and settings)
//!   and owns GPU [`TextureHandle`] caches.
//! - **Network Worker (`network_worker`)**: One background thread per active or
//!   background character session (`BackgroundSession`), communicating with the UI
//!   thread via [`UiToNet`] and [`NetToUi`] `mpsc` channels. Exits cleanly when its
//!   `UiToNet` sender is dropped.
//! - **Asset & Sprite Caches ([`FoxSpriteCache`])**: Lazily decode `FOX5`/`FSHX`/`FSH`
//!   archives into GPU textures with bounded eviction so long-running sessions do not
//!   exhaust RAM or VRAM.
//! - **On-Demand Downloaders**: Dreams (`.rch`/`.zip`/`.map`), species `.fox` patches,
//!   custom portraits (`hades.furcadia.com:6500` + `FPRT10` LZW / `FPRT20`), and inline
//!   chat images are fetched on short-lived worker threads with strict protocol and
//!   size limits.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

#[path = "../logo.rs"]
mod logo;

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound on cached colorized avatar textures per `FoxSpriteCache`.
const MAX_AVATAR_TEXTURES: usize = 512;
/// Upper bound on cached custom portrait RGBA buffers / GPU textures.
const MAX_CUSTOM_PORTRAITS: usize = 256;
/// Upper bound on cached inline chat image GPU textures.
const MAX_CHAT_IMG_TEXTURES: usize = 128;
/// Upper bound on retained chat / whisper lines per tab in memory (full history is in disk logs).
const MAX_CHAT_LINES: usize = 3000;
/// Upper bound on decompressed `FPRT10` LZW portrait bytes (4 MiB).
const MAX_LZW_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Upper bound on decompressed bzip2 asset bytes in the GUI (32 MiB).
const MAX_BZIP2_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

/// Appends a [`ChatLine`] to a log vector while keeping memory bounded to [`MAX_CHAT_LINES`].
fn push_bounded_chat_line(log: &mut Vec<ChatLine>, line: ChatLine) {
    log.push(line);
    if log.len() > MAX_CHAT_LINES {
        let excess = log.len() - MAX_CHAT_LINES;
        log.drain(..excess);
    }
}

use eframe::egui::{
    self, Color32, ColorImage, FontId, Pos2, Rect, RichText, Sense, Stroke, TextureHandle,
    TextureOptions, Vec2,
};
use furcadia_client::{
    decode_fsh_sprites, extract_fr01_to_dir, fetch_account_characters, fetch_news, is_fr01,
    logs::{
        append_chat_log, append_shared_chat_log, append_shared_whisper_log, append_whisper_log,
        default_logs_dir, default_logs_root, list_log_characters, list_log_files,
        load_shared_whisper_log, load_whisper_log, peer_shortname_key, prune_old_logs,
        SHARED_LOG_GROUP,
    },
    parse_chat_line,
    tray::{spawn_tray, TrayAction, TrayHandle},
    unescape_html,
    win_import::{discover_default_windows_furcadia_dirs, import_windows_furcadia_dir},
    AccountInfo, AssetLocator, Avatar, AvatarAppearance, ChatLine, ChatSpan, CharacterSummary,
    ClientCommand, ClientConfig, ConnectionConfig, FoxArchive, FoxAvatarShape, FoxFrame, FoxLayer,
    FurcadiaClient, FurcadiaMap, LookedPlayerInfo, NewsEntry, OnlineStatus, Palette, PounceEvent,
    PounceMonitor, RegionLayer, RemapLuts, ServerMessage, SessionState, WorldState, DEFAULT_HOST,
    DEFAULT_PORT,
};

const FONT_FAMILY_UI_BOLD: &str = "ui_bold";
const MAX_SYSTEM_FONT_BYTES: u64 = 32 * 1024 * 1024;

/// Validates that `bytes` is a single static SFNT (`.ttf`/`.otf`) font file
/// supported cleanly by `epaint` / `ab_glyph` (rejecting `.ttc` collections,
/// variable `fvar` fonts, and malformed tables).
fn is_valid_static_sfnt(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    let magic = &bytes[0..4];
    if magic != [0x00, 0x01, 0x00, 0x00] && magic != b"true" && magic != b"OTTO" {
        return false;
    }
    let num_tables = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
    if num_tables == 0 {
        return false;
    }
    let Some(dir_end) = 12usize.checked_add(num_tables.saturating_mul(16)) else {
        return false;
    };
    if dir_end > bytes.len() {
        return false;
    }

    let mut has_head = false;
    let mut has_hhea = false;
    let mut has_maxp = false;
    let mut has_cmap = false;
    let mut has_outline = false;

    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = &bytes[rec..rec + 4];
        let table_off = u32::from_be_bytes([
            bytes[rec + 8],
            bytes[rec + 9],
            bytes[rec + 10],
            bytes[rec + 11],
        ]);
        let table_len = u32::from_be_bytes([
            bytes[rec + 12],
            bytes[rec + 13],
            bytes[rec + 14],
            bytes[rec + 15],
        ]);
        if tag == b"fvar" {
            return false;
        }
        let Some(end) = table_off.checked_add(table_len) else {
            return false;
        };
        if (end as usize) > bytes.len() {
            return false;
        }
        match tag {
            b"head" => {
                if table_len < 54 {
                    return false;
                }
                let off = table_off as usize;
                let upm = u16::from_be_bytes([bytes[off + 18], bytes[off + 19]]);
                if !(16..=16384).contains(&upm) {
                    return false;
                }
                has_head = true;
            }
            b"hhea" => has_hhea = true,
            b"maxp" => has_maxp = true,
            b"cmap" => has_cmap = true,
            b"glyf" | b"CFF " => has_outline = true,
            _ => {}
        }
    }

    has_head && has_hhea && has_maxp && has_cmap && has_outline
}

fn load_valid_font_file(path: &std::path::Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SYSTEM_FONT_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if is_valid_static_sfnt(&bytes) {
        Some(bytes)
    } else {
        None
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn fc_match_font_path(pattern: &str) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path_str = String::from_utf8(out.stdout).ok()?;
    let trimmed = path_str.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(trimmed))
    }
}

/// Discovers paired `(regular, bold)` static UI fonts from the host OS so `egui`
/// renders crisp sans-serif text instead of its thin `Ubuntu-Light` default.
fn discover_system_ui_fonts() -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let pairs: &[(&[&str], &[&str])] = &[
            (
                &[
                    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
                    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                    "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
                ],
                &[
                    "/usr/share/fonts/noto/NotoSans-SemiBold.ttf",
                    "/usr/share/fonts/noto/NotoSans-Medium.ttf",
                    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
                    "/usr/share/fonts/truetype/noto/NotoSans-SemiBold.ttf",
                    "/usr/share/fonts/truetype/noto/NotoSans-Medium.ttf",
                    "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
                    "/usr/share/fonts/google-noto/NotoSans-SemiBold.ttf",
                    "/usr/share/fonts/google-noto/NotoSans-Medium.ttf",
                    "/usr/share/fonts/google-noto/NotoSans-Bold.ttf",
                ],
            ),
            (
                &[
                    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
                    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                    "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
                ],
                &[
                    "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
                    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
                    "/usr/share/fonts/truetype/liberation2/LiberationSans-Bold.ttf",
                ],
            ),
            (
                &[
                    "/usr/share/fonts/TTF/DejaVuSans.ttf",
                    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
                ],
                &[
                    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
                    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
                    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
                ],
            ),
            (
                &[
                    "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
                    "/usr/share/fonts/ubuntu/Ubuntu-R.ttf",
                    "/usr/share/fonts/TTF/Ubuntu-Regular.ttf",
                ],
                &[
                    "/usr/share/fonts/truetype/ubuntu/Ubuntu-B.ttf",
                    "/usr/share/fonts/ubuntu/Ubuntu-B.ttf",
                    "/usr/share/fonts/TTF/Ubuntu-Bold.ttf",
                ],
            ),
            (
                &[
                    "/usr/share/fonts/TTF/Inter-Regular.ttf",
                    "/usr/share/fonts/inter/Inter-Regular.ttf",
                ],
                &[
                    "/usr/share/fonts/TTF/Inter-SemiBold.ttf",
                    "/usr/share/fonts/TTF/Inter-Medium.ttf",
                    "/usr/share/fonts/TTF/Inter-Bold.ttf",
                    "/usr/share/fonts/inter/Inter-SemiBold.ttf",
                    "/usr/share/fonts/inter/Inter-Bold.ttf",
                ],
            ),
        ];

        for (reg_paths, bold_paths) in pairs {
            if let Some(reg) = reg_paths
                .iter()
                .find_map(|p| load_valid_font_file(std::path::Path::new(p)))
            {
                let bold = bold_paths
                    .iter()
                    .find_map(|p| load_valid_font_file(std::path::Path::new(p)));
                return (Some(reg), bold);
            }
        }

        let reg = fc_match_font_path("sans-serif:weight=regular")
            .and_then(|p| load_valid_font_file(&p));
        let bold = ["sans-serif:weight=semibold", "sans-serif:weight=bold"]
            .iter()
            .find_map(|pat| fc_match_font_path(pat).and_then(|p| load_valid_font_file(&p)));
        return (reg, bold);
    }

    #[cfg(target_os = "windows")]
    {
        let fonts_dir = std::env::var_os("WINDIR")
            .map(|w| std::path::PathBuf::from(w).join("Fonts"))
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\Fonts"));
        let pairs: &[(&str, &[&str])] = &[
            ("segoeui.ttf", &["seguisb.ttf", "segoeuib.ttf"]),
            ("arial.ttf", &["arialbd.ttf"]),
            ("tahoma.ttf", &["tahomabd.ttf"]),
        ];
        for (reg_file, bold_files) in pairs {
            if let Some(reg) = load_valid_font_file(&fonts_dir.join(reg_file)) {
                let bold = bold_files
                    .iter()
                    .find_map(|f| load_valid_font_file(&fonts_dir.join(f)));
                return (Some(reg), bold);
            }
        }
        return (None, None);
    }

    #[cfg(target_os = "macos")]
    {
        let pairs: &[(&str, &[&str])] = &[
            (
                "/System/Library/Fonts/Supplemental/Arial.ttf",
                &["/System/Library/Fonts/Supplemental/Arial Bold.ttf"],
            ),
            (
                "/System/Library/Fonts/Supplemental/Verdana.ttf",
                &["/System/Library/Fonts/Supplemental/Verdana Bold.ttf"],
            ),
            (
                "/Library/Fonts/Arial.ttf",
                &["/Library/Fonts/Arial Bold.ttf"],
            ),
        ];
        for (reg_path, bold_paths) in pairs {
            if let Some(reg) = load_valid_font_file(std::path::Path::new(reg_path)) {
                let bold = bold_paths
                    .iter()
                    .find_map(|p| load_valid_font_file(std::path::Path::new(p)));
                return (Some(reg), bold);
            }
        }
        return (None, None);
    }
}

/// Builds `egui::FontDefinitions` with optional system regular/bold UI fonts and
/// Fixedsys while keeping builtin fallbacks intact.
fn build_font_definitions(
    regular: Option<Vec<u8>>,
    bold: Option<Vec<u8>>,
    fixedsys: Option<Vec<u8>>,
) -> (egui::FontDefinitions, bool) {
    let mut defs = egui::FontDefinitions::default();
    let mut has_regular = false;
    let mut has_bold = false;

    if let Some(bytes) = regular.filter(|b| is_valid_static_sfnt(b)) {
        defs.font_data
            .insert("ui_regular".to_owned(), egui::FontData::from_owned(bytes));
        if let Some(prop) = defs.families.get_mut(&egui::FontFamily::Proportional) {
            prop.insert(0, "ui_regular".to_owned());
        }
        if let Some(mono) = defs.families.get_mut(&egui::FontFamily::Monospace) {
            let idx = usize::from(!mono.is_empty());
            mono.insert(idx, "ui_regular".to_owned());
        }
        has_regular = true;
    }

    if let Some(bytes) = bold.filter(|b| is_valid_static_sfnt(b)) {
        defs.font_data.insert(
            FONT_FAMILY_UI_BOLD.to_owned(),
            egui::FontData::from_owned(bytes),
        );
        let mut chain = vec![FONT_FAMILY_UI_BOLD.to_owned()];
        if has_regular {
            chain.push("ui_regular".to_owned());
        }
        chain.push("Ubuntu-Light".to_owned());
        chain.push("NotoEmoji-Regular".to_owned());
        chain.push("emoji-icon-font".to_owned());
        defs.families
            .insert(egui::FontFamily::Name(FONT_FAMILY_UI_BOLD.into()), chain);
        has_bold = true;
    }

    if let Some(bytes) = fixedsys.filter(|b| is_valid_static_sfnt(b)) {
        defs.font_data
            .insert("fixedsys".to_owned(), egui::FontData::from_owned(bytes));
        let mut chain = vec!["fixedsys".to_owned()];
        if has_regular {
            chain.push("ui_regular".to_owned());
        }
        chain.push("Ubuntu-Light".to_owned());
        chain.push("NotoEmoji-Regular".to_owned());
        chain.push("emoji-icon-font".to_owned());
        defs.families
            .insert(egui::FontFamily::Name("fixedsys".into()), chain);
    }

    (defs, has_bold)
}

fn configure_text_styles(
    ctx: &egui::Context,
    font_size: f32,
    family: egui::FontFamily,
    heading_family: egui::FontFamily,
) {
    let sz = font_size.clamp(10.0, 28.0).round();
    let small_sz = (sz - 2.0).max(10.0).round();
    let heading_sz = (sz + 4.0).min(30.0).round();
    ctx.style_mut(|style| {
        style.text_styles.insert(
            egui::TextStyle::Body,
            FontId::new(sz, family.clone()),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            FontId::new(sz, family.clone()),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            FontId::new(small_sz, family.clone()),
        );
        style.text_styles.insert(
            egui::TextStyle::Heading,
            FontId::new(heading_sz, heading_family),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            FontId::new(sz, egui::FontFamily::Monospace),
        );
    });
}

const BG_PURPLE: Color32 = Color32::from_rgb(23, 14, 46);
const CARD_PURPLE: Color32 = Color32::from_rgb(34, 22, 68);
const INPUT_PURPLE: Color32 = Color32::from_rgb(20, 12, 40);
const GOLD: Color32 = Color32::from_rgb(212, 169, 74);
const GOLD_DIM: Color32 = Color32::from_rgb(165, 130, 62);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppScreen {
    Login,
    CharacterSelect,
    Connecting,
    InGame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharSort {
    Recent,
    Alphabetical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarTab {
    Move,
    GoTo,
    Edit,
    Digo,
    Emotes,
    Misc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DreamTheme {
    ClassicPurple,
    DragonHoard,
    KitterMagic,
    FerianWylde,
}

impl DreamTheme {
    fn label(self) -> &'static str {
        match self {
            Self::ClassicPurple => "Classic Purple",
            Self::DragonHoard => "DragonHoard",
            Self::KitterMagic => "KitterMagic",
            Self::FerianWylde => "FerianWylde",
        }
    }

    fn bg(self) -> Color32 {
        match self {
            Self::ClassicPurple => BG_PURPLE,
            Self::DragonHoard => Color32::from_rgb(38, 16, 16),
            Self::KitterMagic => Color32::from_rgb(20, 26, 48),
            Self::FerianWylde => Color32::from_rgb(18, 34, 24),
        }
    }

    fn panel(self) -> Color32 {
        match self {
            Self::ClassicPurple => CARD_PURPLE,
            Self::DragonHoard => Color32::from_rgb(56, 24, 22),
            Self::KitterMagic => Color32::from_rgb(30, 40, 72),
            Self::FerianWylde => Color32::from_rgb(28, 50, 36),
        }
    }

    fn accent(self) -> Color32 {
        match self {
            Self::ClassicPurple | Self::DragonHoard => GOLD,
            Self::KitterMagic => Color32::from_rgb(135, 205, 250),
            Self::FerianWylde => Color32::from_rgb(155, 220, 120),
        }
    }
}

#[derive(Debug, Clone)]
struct UpdateReleaseInfo {
    tag: String,
    version: String,
    html_url: String,
    asset_url: Option<String>,
}

enum UpdateCheckMsg {
    UpToDate(String),
    UpdateAvailable(UpdateReleaseInfo),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdateInstallState {
    Idle,
    Downloading,
    Installing,
    Installed,
    Failed(String),
}

enum UpdateInstallMsg {
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    Installing,
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    Installed,
    #[cfg(target_os = "windows")]
    LaunchInstallerAndExit(std::path::PathBuf),
    #[cfg(target_os = "macos")]
    OpenDmg(std::path::PathBuf),
    Failed(String),
}

fn update_download_filename(url: &str) -> String {
    let base = url.split('?').next().unwrap_or(url);
    let name = base.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() {
        "furcadia-native-update".to_owned()
    } else {
        name.to_owned()
    }
}

fn extract_json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let idx = json.find(&needle)?;
    let rest = &json[idx + needle.len()..];
    let colon = rest.find(':')?;
    let after_colon = rest[colon + 1..].trim_start();
    let content = after_colon.strip_prefix('"')?;
    let mut out = String::new();
    let mut escaped = false;
    for ch in content.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn parse_version_tuple(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    let core = trimmed.split(['-', '+']).next().unwrap_or(trimmed);
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    let patch = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    Some((major, minor, patch))
}

fn is_newer_version(current: &str, latest: &str) -> bool {
    match (parse_version_tuple(current), parse_version_tuple(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

#[cfg(any(test, not(any(target_os = "windows", target_os = "macos"))))]
fn select_linux_asset_url(urls: &[String], is_arch: bool, is_rpm: bool) -> Option<String> {
    if is_arch {
        urls.iter()
            .find(|u| u.ends_with(".pkg.tar.zst") && !u.contains("-debug-"))
            .cloned()
    } else if is_rpm {
        urls.iter().find(|u| u.ends_with(".rpm")).cloned()
    } else {
        urls.iter().find(|u| u.ends_with(".deb")).cloned()
    }
}

fn parse_github_release_json(json: &str) -> Option<UpdateReleaseInfo> {
    let tag = extract_json_string_field(json, "tag_name")?;
    let version = tag.trim().trim_start_matches(['v', 'V']).to_owned();
    let html_url = extract_json_string_field(json, "html_url")
        .unwrap_or_else(|| "https://github.com/morganlowe/native-furcadia/releases".to_owned());

    let mut urls = Vec::new();
    let mut search = json;
    while let Some(pos) = search.find("\"browser_download_url\"") {
        let slice = &search[pos..];
        if let Some(url) = extract_json_string_field(slice, "browser_download_url") {
            urls.push(url);
        }
        search = &search[pos + "\"browser_download_url\"".len()..];
    }

    #[cfg(target_os = "windows")]
    let asset_url = urls
        .iter()
        .find(|u| u.to_ascii_lowercase().ends_with(".exe"))
        .cloned();
    #[cfg(target_os = "macos")]
    let asset_url = urls
        .iter()
        .find(|u| u.to_ascii_lowercase().ends_with(".dmg"))
        .cloned();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let asset_url = {
        let is_arch = std::path::Path::new("/etc/arch-release").exists();
        let is_rpm = std::path::Path::new("/etc/fedora-release").exists()
            || std::path::Path::new("/etc/redhat-release").exists()
            || std::path::Path::new("/etc/SuSE-release").exists()
            || std::path::Path::new("/etc/SUSE-brand").exists()
            || (std::path::Path::new("/usr/bin/rpm").exists()
                && !std::path::Path::new("/usr/bin/dpkg").exists());
        select_linux_asset_url(&urls, is_arch, is_rpm)
    };

    Some(UpdateReleaseInfo {
        tag,
        version,
        html_url,
        asset_url,
    })
}

struct WhisperSession {
    peer: String,
    history_lines: Vec<String>,
    lines: Vec<ChatLine>,
    input: String,
    unread: usize,
    detached: bool,
    looked_card: Option<LookedPlayerCard>,
    portrait_fetched: bool,
}

impl WhisperSession {
    fn new(peer: String) -> Self {
        Self {
            peer,
            history_lines: Vec::new(),
            lines: Vec::new(),
            input: String::new(),
            unread: 0,
            detached: false,
            looked_card: None,
            portrait_fetched: false,
        }
    }
}

/// Renders persisted whisper-log history lines above the live whisper stream.
#[allow(clippy::too_many_arguments)] // Passes read-only UI/theme state directly to avoid a wrapper struct.
fn render_whisper_history_lines(
    ui: &mut egui::Ui,
    history_lines: &[String],
    has_live_lines: bool,
    self_name: &str,
    peer: &str,
    action_out: &mut Option<ChatClickAction>,
    smileys: &[Option<TextureHandle>],
    desctags: &[Option<TextureHandle>],
    chat_imgs: &HashMap<String, Option<TextureHandle>>,
    player_badge_colors: &HashMap<String, Color32>,
    player_badges: &HashMap<String, TextureHandle>,
    self_name_key: &str,
    colors: &UiColors,
) {
    if history_lines.is_empty() {
        return;
    }
    let log_color = Color32::from_rgb(142, 145, 160);
    let hist_sz = ui.style().text_styles[&egui::TextStyle::Body].size.round();
    for line in history_lines {
        let mut parts = line.splitn(3, ' ');
        let date = parts.next().unwrap_or_default();
        let time = parts.next().unwrap_or_default();
        let message = parts.next();
        if let Some(message) = message {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{date} {time}"))
                        .size(hist_sz)
                        .color(log_color),
                );
                let parsed = parse_chat_line(message);
                let formatted = format_whisper_tab_line(self_name, peer, message, &parsed);
                render_chat_line(
                    ui,
                    &formatted,
                    action_out,
                    smileys,
                    desctags,
                    chat_imgs,
                    player_badge_colors,
                    player_badges,
                    self_name_key,
                    colors,
                );
            });
        } else {
            ui.add(egui::Label::new(RichText::new(line).size(hist_sz).color(log_color)).wrap());
        }
    }
    if has_live_lines {
        ui.label(
            RichText::new("─── Current Session ───")
                .size(11.0)
                .color(Color32::from_rgb(108, 110, 126)),
        );
    }
}

#[derive(Clone)]
struct CachedSprite {
    tex: TextureHandle,
    offset: (i16, i16),
    elev: (i16, i16),
    is_one_by_one: bool,
}

#[derive(Clone)]
struct ResolvedLayerTex {
    tex: TextureHandle,
    offset: (i16, i16),
    is_one_by_one: bool,
    category: u16,
    slot: u8,
}

#[derive(Clone)]
struct ResolvedShapeLayers {
    layers: Vec<ResolvedLayerTex>,
    elev: (i16, i16),
    effect_pass: u8,
    alpha: u8,
    is_animated: bool,
}

#[derive(Clone)]
struct AvatarHitBox {
    uid: u32,
    display_name: String,
    short_name: String,
    is_self: bool,
    sprite_rect: Option<Rect>,
    badge_rect: Option<Rect>,
    fallback_center: Option<Pos2>,
    fallback_radius: f32,
}

impl AvatarHitBox {
    fn contains_badge(&self, pos: Pos2) -> bool {
        self.badge_rect.is_some_and(|r| r.contains(pos))
    }

    fn contains_body(&self, pos: Pos2) -> bool {
        self.sprite_rect.is_some_and(|r| r.contains(pos))
            || self
                .fallback_center
                .is_some_and(|c| (pos - c).length() <= self.fallback_radius)
    }
}

#[derive(Clone)]
struct NameBadge {
    uid: u32,
    pos: Pos2,
    name: String,
    is_self: bool,
}

#[derive(Clone)]
struct ContextMenuTarget {
    uid: u32,
    display_name: String,
    short_name: String,
    is_self: bool,
    anchor_pos: Pos2,
    opened_this_frame: bool,
}

#[derive(Clone)]
struct LookedPlayerCard {
    info: LookedPlayerInfo,
    short_name: String,
    portrait_id: Option<u32>,
    portrait_tex: Option<TextureHandle>,
    badge_tex: Option<TextureHandle>,
    resolved_for: Option<(u16, u8, [u8; 12])>,
}

#[derive(Clone)]
struct SilentTabGlook {
    target_key: String,
    sent_at: Instant,
    card_received: bool,
    saw_you_see: bool,
}

#[derive(Clone, Copy)]
enum PlayerMenuAction {
    Look,
    Whisper,
    Join,
    Summon,
    Follow,
    Lead,
    Cuddle,
    TogglePounce,
    ToggleIgnore,
    ShareEdit,
    UnshareEdit,
    Raw(&'static str),
}

fn render_player_menu_buttons(
    ui: &mut egui::Ui,
    is_self: bool,
    in_pounce: bool,
    pounce_only: bool,
) -> Option<PlayerMenuAction> {
    let mut chosen = None;
    let mut item = |ui: &mut egui::Ui, label: &str, act: PlayerMenuAction| {
        if ui
            .add(egui::Button::new(label).min_size(Vec2::new(146.0, 22.0)))
            .clicked()
        {
            chosen = Some(act);
        }
    };
    if is_self {
        item(ui, "Look (Self)", PlayerMenuAction::Look);
        ui.separator();
        for (label, cmd) in [
            ("Stand", "stand"),
            ("Sit", "sit"),
            ("Lay Down", "liedown"),
            ("Turn Left", "<"),
            ("Turn Right", ">"),
            ("Get / Drop", "get"),
            ("Use", "use"),
            ("Wings", "wings"),
            ("Costume", "costume"),
            ("Eat Cookie", "eat-cookie"),
        ] {
            item(ui, label, PlayerMenuAction::Raw(cmd));
        }
    } else if pounce_only {
        item(ui, "Whisper", PlayerMenuAction::Whisper);
        item(ui, "Look", PlayerMenuAction::Look);
        ui.separator();
        item(ui, "Join", PlayerMenuAction::Join);
        item(ui, "Summon", PlayerMenuAction::Summon);
        item(ui, "Follow", PlayerMenuAction::Follow);
        item(ui, "Cuddle", PlayerMenuAction::Cuddle);
        ui.separator();
        item(ui, "Ignore / Unignore", PlayerMenuAction::ToggleIgnore);
        item(ui, "Remove", PlayerMenuAction::TogglePounce);
    } else {
        item(ui, "Look", PlayerMenuAction::Look);
        item(ui, "Whisper", PlayerMenuAction::Whisper);
        ui.separator();
        item(ui, "Join", PlayerMenuAction::Join);
        item(ui, "Summon", PlayerMenuAction::Summon);
        item(ui, "Follow", PlayerMenuAction::Follow);
        item(ui, "Lead", PlayerMenuAction::Lead);
        item(ui, "Cuddle", PlayerMenuAction::Cuddle);
        ui.separator();
        item(
            ui,
            if in_pounce {
                "Remove from Pounce"
            } else {
                "Add to Pounce"
            },
            PlayerMenuAction::TogglePounce,
        );
        item(ui, "Ignore / Unignore", PlayerMenuAction::ToggleIgnore);
        item(ui, "Share Edit", PlayerMenuAction::ShareEdit);
        item(ui, "Unshare Edit", PlayerMenuAction::UnshareEdit);
    }
    chosen
}

fn wire_player_name(name: &str) -> String {
    name.trim().replace(' ', "|")
}

fn sanitized_log_character_name(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | ' ' => c,
            _ => '_',
        })
        .collect()
}

fn shortname_key(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.2} GB", b / GB)
    }
}

/// Maps a wire species ID to `(cache_file_id, object_id)` matching the official web client
/// (`Eu()` for stock/local species and `Vu()`/`]M%` for dynamic avatars `DPlayer<id-135>.fox`).
fn resolve_species_spec_with_dynamic(
    sp_id: usize,
    dynamic_avatars: &HashMap<u16, (usize, bool)>,
) -> (usize, u16) {
    if (1000..=2000).contains(&sp_id) {
        return (sp_id, 0);
    }
    match sp_id {
        0 => return (1, 0),
        97 => return (104, 1),
        98 => return (106, 1),
        99 => return (119, 1),
        _ => {}
    }
    let (base_sp, obj_id) = if sp_id > 2000 {
        (sp_id % 2000, (sp_id / 2000) as u16)
    } else {
        (sp_id, 0)
    };
    if base_sp == 0 {
        return (1, 0);
    }
    if (1000..=2000).contains(&base_sp) {
        return (base_sp, obj_id);
    }
    if let Ok(sp_u16) = u16::try_from(base_sp) {
        if let Some(&(dplayer_id, _hop)) = dynamic_avatars.get(&sp_u16) {
            return (DYNAMIC_AVATAR_CACHE_BASE + dplayer_id, obj_id);
        }
    }
    if (136..1000).contains(&base_sp) {
        return (DYNAMIC_AVATAR_CACHE_BASE + (base_sp - 135), obj_id);
    }
    (base_sp, obj_id)
}

/// `(species_id, dplayer_id, hop)` entry parsed from `]M%` dynamic avatar metadata.
type DynamicAvatarEntry = (u16, usize, bool);

/// Parses `]M%<server_code><8-byte entries>` (`DynamicAvatarDetails`).
/// Returns `(server_code, Vec<DynamicAvatarEntry>)`.
fn parse_dynamic_avatar_details(data: &[u8]) -> Option<(Option<char>, Vec<DynamicAvatarEntry>)> {
    if data.first() != Some(&b'%') || data.len() < 2 {
        return None;
    }
    let srv = data[1];
    let server_code = (srv != b' ' && srv.is_ascii_alphanumeric()).then_some(srv as char);
    let mut out = Vec::new();
    for chunk in data[2..].as_chunks::<8>().0 {
        let lo = chunk[2].saturating_sub(35) as u16;
        let hi_and_flag = chunk[3].saturating_sub(35) as u16;
        let species_id = (hi_and_flag >> 1) * 220 + lo;
        let hop = (hi_and_flag & 1) != 0;
        if (136..1000).contains(&species_id) {
            let dplayer_id = (species_id - 135) as usize;
            out.push((species_id, dplayer_id, hop));
        }
    }
    Some((server_code, out))
}

/// Cache-key offset for dynamic avatars (`DPlayer<id>.fox`) so they never collide with `species<id>.fox`.
const DYNAMIC_AVATAR_CACHE_BASE: usize = 100_000;

const ARCHIVE_DEFAULT: u8 = 0;
const ARCHIVE_DREAM: u8 = 1;
const ARCHIVE_LEGACY: u8 = 2;

/// Identifies a stock world patch archive from Furcadia's official asset host
/// (`https://play.furcadia.com/{modern|legacy}/<file>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StockPatchSlot {
    ModernFloors,
    LegacyFloors,
    LegacyFloors2,
    ModernFloore,
    LegacyFloore,
    ModernItems,
    LegacyItem,
    LegacyItem2,
    LegacyItem3,
    LegacyItem4,
    ModernIteme,
    LegacyIteme,
    ModernPortals,
    ModernPortalse,
    ModernEffects,
    LegacyEffects,
    ModernEffectse,
    LegacyEffectse,
    ModernJunction,
    LegacyJunction,
    ModernWall(usize),
    LegacyWall(usize),
}

impl StockPatchSlot {
    fn is_valid(self) -> bool {
        match self {
            Self::ModernWall(g) | Self::LegacyWall(g) => g == 0 || (3..=7).contains(&g),
            _ => true,
        }
    }

    fn is_legacy(self) -> bool {
        matches!(
            self,
            Self::LegacyFloors
                | Self::LegacyFloors2
                | Self::LegacyFloore
                | Self::LegacyItem
                | Self::LegacyItem2
                | Self::LegacyItem3
                | Self::LegacyItem4
                | Self::LegacyIteme
                | Self::LegacyEffects
                | Self::LegacyEffectse
                | Self::LegacyJunction
                | Self::LegacyWall(_)
        )
    }

    fn subdir(self) -> &'static str {
        if self.is_legacy() {
            "legacy"
        } else {
            "modern"
        }
    }

    fn filename(self) -> String {
        match self {
            Self::ModernFloors | Self::LegacyFloors => "floors.fox".to_owned(),
            Self::LegacyFloors2 => "floors2.fox".to_owned(),
            Self::ModernFloore | Self::LegacyFloore => "floore.fox".to_owned(),
            Self::ModernItems => "items.fox".to_owned(),
            Self::LegacyItem => "item.fox".to_owned(),
            Self::LegacyItem2 => "item2.fox".to_owned(),
            Self::LegacyItem3 => "item3.fox".to_owned(),
            Self::LegacyItem4 => "item4.fox".to_owned(),
            Self::ModernIteme | Self::LegacyIteme => "iteme.fox".to_owned(),
            Self::ModernPortals => "portals.fox".to_owned(),
            Self::ModernPortalse => "portalse.fox".to_owned(),
            Self::ModernEffects | Self::LegacyEffects => "effects.fox".to_owned(),
            Self::ModernEffectse | Self::LegacyEffectse => "effectse.fox".to_owned(),
            Self::ModernJunction | Self::LegacyJunction => "junction.fox".to_owned(),
            Self::ModernWall(0) | Self::LegacyWall(0) => "wall.fox".to_owned(),
            Self::ModernWall(g) | Self::LegacyWall(g) => format!("wall_{g}.fox"),
        }
    }
}

/// Layered sprite & GPU texture cache for a single Furcadia patch category
/// (floors, items, walls, portals, effects, or a species/dynamic avatar).
///
/// Lookup priority is `dream_archive` -> `default_archive` -> `legacy_archive`,
/// matching how Furcadia dream patches override stock artwork. Colorized avatar
/// textures are bounded by [`MAX_AVATAR_TEXTURES`] to prevent VRAM growth.
struct FoxSpriteCache {
    default_archive: Option<FoxArchive>,
    legacy_archive: Option<FoxArchive>,
    dream_archive: Option<FoxArchive>,
    shape_textures: HashMap<(u16, u8, bool), Option<CachedSprite>>,
    image_textures: HashMap<(u8, usize), (TextureHandle, bool)>,
    avatar_textures: HashMap<(bool, usize, [u8; 12]), (TextureHandle, bool)>,
    prefix: String,
}

impl FoxSpriteCache {
    fn load(assets: &AssetLocator, filename: &str, prefix: &str) -> Self {
        Self::load_with_legacy(assets, filename, None, prefix)
    }

    fn load_with_legacy(
        assets: &AssetLocator,
        default_filename: &str,
        legacy_filename: Option<&str>,
        prefix: &str,
    ) -> Self {
        let single: [&str; 1];
        let legacy_candidates: &[&str] = match legacy_filename {
            Some(f) => {
                single = [f];
                &single
            }
            None => &[],
        };
        Self::load_with_legacy_candidates(assets, default_filename, legacy_candidates, prefix)
    }

    fn load_with_legacy_candidates(
        assets: &AssetLocator,
        default_filename: &str,
        legacy_filenames: &[&str],
        prefix: &str,
    ) -> Self {
        let default_archive = assets
            .default_patch_path(default_filename)
            .and_then(|p| FoxArchive::load(&p).ok());
        let legacy_archive = legacy_filenames
            .iter()
            .find_map(|f| assets.legacy_patch_path(f))
            .and_then(|p| FoxArchive::load(&p).ok());
        Self {
            default_archive,
            legacy_archive,
            dream_archive: None,
            shape_textures: HashMap::new(),
            image_textures: HashMap::new(),
            avatar_textures: HashMap::new(),
            prefix: prefix.to_owned(),
        }
    }

    fn load_legacy_only(assets: &AssetLocator, legacy_filename: &str, prefix: &str) -> Self {
        let legacy_archive = assets
            .legacy_patch_path(legacy_filename)
            .and_then(|p| FoxArchive::load(&p).ok());
        Self {
            default_archive: None,
            legacy_archive,
            dream_archive: None,
            shape_textures: HashMap::new(),
            image_textures: HashMap::new(),
            avatar_textures: HashMap::new(),
            prefix: prefix.to_owned(),
        }
    }

    fn load_local_species(assets: &AssetLocator, local_idx: usize, prefix: &str) -> Self {
        let default_archive = assets
            .local_species_patch_path(local_idx)
            .and_then(|p| FoxArchive::load(&p).ok());
        Self {
            default_archive,
            legacy_archive: None,
            dream_archive: None,
            shape_textures: HashMap::new(),
            image_textures: HashMap::new(),
            avatar_textures: HashMap::new(),
            prefix: prefix.to_owned(),
        }
    }

    fn from_dream_only(path: &std::path::Path, prefix: &str) -> Self {
        Self {
            default_archive: None,
            legacy_archive: None,
            dream_archive: FoxArchive::load(path).ok(),
            shape_textures: HashMap::new(),
            image_textures: HashMap::new(),
            avatar_textures: HashMap::new(),
            prefix: prefix.to_owned(),
        }
    }

    fn clear_cached_textures(&mut self) {
        self.shape_textures.clear();
        self.image_textures.clear();
        self.avatar_textures.clear();
    }

    fn set_dream_patch(&mut self, path: &std::path::Path) -> Result<(), String> {
        match FoxArchive::load(path) {
            Ok(arc) => {
                self.dream_archive = Some(arc);
                self.clear_cached_textures();
                Ok(())
            }
            Err(e) => {
                self.dream_archive = None;
                self.shape_textures.clear();
                self.image_textures.clear();
                Err(format!(
                    "{}: {e}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("patch.fox")
                ))
            }
        }
    }

    fn clear_dream_patch(&mut self) {
        if self.dream_archive.take().is_some() {
            self.clear_cached_textures();
        }
    }

    fn set_default_archive(&mut self, arc: FoxArchive) {
        self.default_archive = Some(arc);
        self.clear_cached_textures();
    }

    fn set_legacy_archive(&mut self, arc: FoxArchive) {
        self.legacy_archive = Some(arc);
        self.clear_cached_textures();
    }

    fn clear_base_archives(&mut self) {
        let had_default = self.default_archive.take().is_some();
        let had_legacy = self.legacy_archive.take().is_some();
        if had_default || had_legacy {
            self.clear_cached_textures();
        }
    }

    fn reload_base_from_assets(
        &mut self,
        assets: &AssetLocator,
        default_filename: Option<&str>,
        legacy_filenames: &[&str],
    ) {
        self.clear_base_archives();
        if let Some(arc) = default_filename
            .and_then(|f| assets.default_patch_path(f))
            .and_then(|p| FoxArchive::load(&p).ok())
        {
            self.set_default_archive(arc);
        }
        if let Some(arc) = legacy_filenames
            .iter()
            .find_map(|f| assets.legacy_patch_path(f))
            .and_then(|p| FoxArchive::load(&p).ok())
        {
            self.set_legacy_archive(arc);
        }
    }

    fn is_loaded(&self) -> bool {
        self.dream_archive.is_some()
            || self.default_archive.is_some()
            || self.legacy_archive.is_some()
    }

    fn has_dream(&self) -> bool {
        self.dream_archive.is_some()
    }

    fn is_loaded_for_mode(&self, use_legacy: bool) -> bool {
        self.dream_archive.is_some()
            || if use_legacy {
                self.legacy_archive.is_some()
            } else {
                self.default_archive.is_some()
            }
    }

    fn active_is_fshx_or_fsh(&self, use_legacy: bool) -> bool {
        let base = if use_legacy {
            self.legacy_archive.as_ref()
        } else {
            self.default_archive.as_ref()
        };
        self.dream_archive
            .as_ref()
            .or(base)
            .is_some_and(|arc| arc.is_fshx_or_fsh)
    }

    fn active_uses_paired_wall_ids(&self, use_legacy: bool) -> bool {
        let base = if use_legacy {
            self.legacy_archive.as_ref()
        } else {
            self.default_archive.as_ref()
        };
        self.dream_archive
            .as_ref()
            .or(base)
            .is_some_and(|arc| arc.uses_paired_wall_ids())
    }

    fn dream_is_fshx_or_fsh(&self) -> bool {
        self.dream_archive
            .as_ref()
            .is_some_and(|arc| arc.is_fshx_or_fsh)
    }

    fn decode_or_get_image(
        fox: &mut FoxArchive,
        archive_tag: u8,
        prefix: &str,
        image_textures: &mut HashMap<(u8, usize), (TextureHandle, bool)>,
        ctx: &egui::Context,
        palette: &Palette,
        image_idx: usize,
    ) -> Option<(TextureHandle, bool)> {
        if let Some(entry) = image_textures.get(&(archive_tag, image_idx)) {
            return Some(entry.clone());
        }
        let sprite = fox.decode_image(image_idx, palette).ok()?;
        if sprite.width == 0 || sprite.height == 0 {
            return None;
        }
        let is_one_by_one = sprite.width <= 1 && sprite.height <= 1;
        let color_img = ColorImage::from_rgba_unmultiplied(
            [sprite.width as usize, sprite.height as usize],
            &sprite.rgba,
        );
        let tag = match archive_tag {
            ARCHIVE_DREAM => "d",
            ARCHIVE_LEGACY => "l",
            _ => "b",
        };
        let tex = ctx.load_texture(
            format!("{prefix}_{tag}_img_{image_idx}"),
            color_img,
            TextureOptions::NEAREST,
        );
        let pair = (tex, is_one_by_one);
        image_textures.insert((archive_tag, image_idx), pair.clone());
        Some(pair)
    }

    /// Decodes and appends all non-placeholder sprite layers for a single FOX frame.
    #[allow(clippy::too_many_arguments)] // Internal sprite-cache helper receiving split borrows of cache fields.
    fn resolve_frame_layers(
        fox: &mut FoxArchive,
        archive_tag: u8,
        prefix: &str,
        image_textures: &mut HashMap<(u8, usize), (TextureHandle, bool)>,
        ctx: &egui::Context,
        palette: &Palette,
        frame: &FoxFrame,
        slot: u8,
        slot_offset: (i16, i16),
        out: &mut Vec<ResolvedLayerTex>,
    ) {
        let mut layers: Vec<FoxLayer> = frame.layers.clone();
        layers.sort_by_key(|l| if (l.category & 0x40) != 0 { 0 } else { 1 });
        for layer in layers {
            let Some((tex, is_one_by_one)) = Self::decode_or_get_image(
                fox,
                archive_tag,
                prefix,
                image_textures,
                ctx,
                palette,
                layer.image_idx,
            ) else {
                continue;
            };
            if is_one_by_one {
                continue;
            }
            out.push(ResolvedLayerTex {
                tex,
                offset: (
                    slot_offset.0.saturating_add(layer.layer_offset.0),
                    slot_offset.1.saturating_add(layer.layer_offset.1),
                ),
                is_one_by_one,
                category: layer.category,
                slot,
            });
        }
    }

    /// Evaluates KitterSpeak/slot animations for a FOX shape and resolves its active frame layers.
    #[allow(clippy::too_many_arguments)] // Internal sprite-cache helper receiving split borrows of cache fields.
    fn resolve_shape_slots(
        fox: &mut FoxArchive,
        archive_tag: u8,
        prefix: &str,
        image_textures: &mut HashMap<(u8, usize), (TextureHandle, bool)>,
        ctx: &egui::Context,
        palette: &Palette,
        shape: &FoxAvatarShape,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        let resolved = shape.slots_at(anim_ms)?;
        // KitterSpeak step programs can animate slot offsets/state even when the
        // shape reuses a single frame, so the presence of a program is what
        // determines whether the view needs periodic redraws.
        let is_animated = !shape.anim_steps.is_empty();
        let mut layers = Vec::new();
        for active in resolved.slots.iter().flatten() {
            let Some(frame) = shape.frames.get(active.frame_idx) else {
                continue;
            };
            Self::resolve_frame_layers(
                fox,
                archive_tag,
                prefix,
                image_textures,
                ctx,
                palette,
                frame,
                active.slot,
                active.offset,
                &mut layers,
            );
        }
        Some(ResolvedShapeLayers {
            layers,
            elev: resolved.elev,
            effect_pass: resolved.effect_pass,
            alpha: resolved.alpha,
            is_animated,
        })
    }

    fn get_or_load_shape_mode(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        dir: u8,
        use_legacy: bool,
    ) -> Option<CachedSprite> {
        if let Some(cached) = self.shape_textures.get(&(id, dir, use_legacy)) {
            return cached.clone();
        }

        let candidates = if use_legacy {
            [
                (ARCHIVE_DREAM, self.dream_archive.as_mut()),
                (ARCHIVE_LEGACY, self.legacy_archive.as_mut()),
                (ARCHIVE_DEFAULT, self.default_archive.as_mut()),
            ]
        } else {
            [
                (ARCHIVE_DREAM, self.dream_archive.as_mut()),
                (ARCHIVE_DEFAULT, self.default_archive.as_mut()),
                (ARCHIVE_LEGACY, self.legacy_archive.as_mut()),
            ]
        };
        let mut loaded: Option<CachedSprite> = None;
        for (archive_tag, maybe_fox) in candidates {
            if let Some(fox) = maybe_fox {
                if let Some(shape) = fox.resolve_shape(id, dir) {
                    if let Some((tex, is_one_by_one)) = Self::decode_or_get_image(
                        fox,
                        archive_tag,
                        &self.prefix,
                        &mut self.image_textures,
                        ctx,
                        palette,
                        shape.image_idx,
                    ) {
                        loaded = Some(CachedSprite {
                            tex,
                            offset: shape.offset,
                            elev: shape.elev,
                            is_one_by_one,
                        });
                        break;
                    }
                }
            }
        }

        self.shape_textures.insert((id, dir, use_legacy), loaded.clone());
        loaded
    }

    /// Resolves a wall shape strictly from the dream patch. A present custom wall
    /// archive replaces that wall group; absent shapes must stay absent instead of
    /// falling through to unrelated default wall art.
    fn get_or_load_dream_shape(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        dir: u8,
    ) -> Option<CachedSprite> {
        let key = (id, dir, false);
        if let Some(cached) = self.shape_textures.get(&key) {
            return cached.clone();
        }
        let mut loaded = None;
        if let Some(fox) = self.dream_archive.as_mut() {
            if let Some(shape) = fox.resolve_shape(id, dir) {
                if let Some((tex, is_one_by_one)) = Self::decode_or_get_image(
                    fox,
                    ARCHIVE_DREAM,
                    &self.prefix,
                    &mut self.image_textures,
                    ctx,
                    palette,
                    shape.image_idx,
                ) {
                    loaded = Some(CachedSprite {
                        tex,
                        offset: shape.offset,
                        elev: shape.elev,
                        is_one_by_one,
                    });
                }
            }
        }
        self.shape_textures.insert(key, loaded.clone());
        loaded
    }

    fn get_or_load_shape_layers(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        dir: u8,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        self.get_or_load_layers_inner(ctx, palette, id, None, None, dir, anim_ms, false)
    }

    fn get_or_load_shape_layers_mode(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        dir: u8,
        anim_ms: u64,
        use_legacy: bool,
    ) -> Option<ResolvedShapeLayers> {
        self.get_or_load_layers_inner(ctx, palette, id, None, None, dir, anim_ms, use_legacy)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_archive_shape_layers(
        fox: Option<&mut FoxArchive>,
        archive_tag: u8,
        prefix: &str,
        image_textures: &mut HashMap<(u8, usize), (TextureHandle, bool)>,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        dir: u8,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        let fox = fox?;
        let shape = fox.resolve_object_shape(id, dir).cloned()?;
        Self::resolve_shape_slots(
            fox,
            archive_tag,
            prefix,
            image_textures,
            ctx,
            palette,
            &shape,
            anim_ms,
        )
    }

    fn get_or_load_dream_shape_layers(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        dir: u8,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        Self::resolve_archive_shape_layers(
            self.dream_archive.as_mut(),
            ARCHIVE_DREAM,
            &self.prefix,
            &mut self.image_textures,
            ctx,
            palette,
            id,
            dir,
            anim_ms,
        )
    }

    fn get_or_load_default_shape_layers(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        dir: u8,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        Self::resolve_archive_shape_layers(
            self.default_archive.as_mut(),
            ARCHIVE_DEFAULT,
            &self.prefix,
            &mut self.image_textures,
            ctx,
            palette,
            id,
            dir,
            anim_ms,
        )
    }

    fn get_or_load_legacy_shape_layers(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        dir: u8,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        Self::resolve_archive_shape_layers(
            self.legacy_archive.as_mut(),
            ARCHIVE_LEGACY,
            &self.prefix,
            &mut self.image_textures,
            ctx,
            palette,
            id,
            dir,
            anim_ms,
        )
    }

    /// Resolves and caches sprite layers for a FOX object addressed by `(id, purpose, state, dir)`.
    #[allow(clippy::too_many_arguments)] // Thin forwarder to `get_or_load_layers_inner`.
    fn get_or_load_ps_layers(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        purpose: u8,
        state: Option<u8>,
        dir: u8,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        self.get_or_load_layers_inner(
            ctx,
            palette,
            id,
            Some(purpose),
            Some(state),
            dir,
            anim_ms,
            false,
        )
    }

    /// Searches dream, default, and legacy FOX archives in priority order and resolves shape layers.
    #[allow(clippy::too_many_arguments)] // Internal lookup helper shared by standard and purpose/state queries.
    fn get_or_load_layers_inner(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        id: u16,
        purpose: Option<u8>,
        state: Option<Option<u8>>,
        dir: u8,
        anim_ms: u64,
        use_legacy: bool,
    ) -> Option<ResolvedShapeLayers> {
        let prefix = self.prefix.clone();
        let candidates = if use_legacy {
            [
                (ARCHIVE_DREAM, self.dream_archive.as_mut()),
                (ARCHIVE_LEGACY, self.legacy_archive.as_mut()),
                (ARCHIVE_DEFAULT, self.default_archive.as_mut()),
            ]
        } else {
            [
                (ARCHIVE_DREAM, self.dream_archive.as_mut()),
                (ARCHIVE_DEFAULT, self.default_archive.as_mut()),
                (ARCHIVE_LEGACY, self.legacy_archive.as_mut()),
            ]
        };
        for (archive_tag, maybe_fox) in candidates {
            let Some(fox) = maybe_fox else {
                continue;
            };
            let shape_opt = match (purpose, state) {
                (Some(p), Some(s)) => fox.resolve_object_shape_ps(id, p, s, dir),
                _ => fox.resolve_object_shape(id, dir),
            };
            let Some(shape) = shape_opt.cloned() else {
                continue;
            };
            if let Some(resolved) = Self::resolve_shape_slots(
                fox,
                archive_tag,
                &prefix,
                &mut self.image_textures,
                ctx,
                palette,
                &shape,
                anim_ms,
            ) {
                return Some(resolved);
            }
        }
        None
    }

    /// Decodes, optionally color-remaps, and caches an individual avatar sprite texture.
    #[allow(clippy::too_many_arguments)] // Internal avatar-cache helper taking split borrows of cache state.
    fn decode_or_get_avatar_image(
        fox: &mut FoxArchive,
        is_dream: bool,
        prefix: &str,
        avatar_textures: &mut HashMap<(bool, usize, [u8; 12]), (TextureHandle, bool)>,
        ctx: &egui::Context,
        palette: &Palette,
        appearance: &AvatarAppearance,
        image_idx: usize,
        remap_color: bool,
    ) -> Option<(TextureHandle, bool)> {
        let cache_color = if remap_color {
            appearance.color_key
        } else {
            [0xff; 12]
        };
        if let Some(cached) = avatar_textures.get(&(is_dream, image_idx, cache_color)) {
            return Some(cached.clone());
        }
        let sprite = fox.decode_image(image_idx, palette).ok()?;
        if sprite.width == 0 || sprite.height == 0 {
            return None;
        }
        let is_one_by_one = sprite.width <= 1 && sprite.height <= 1;
        let pixels = if remap_color {
            RemapLuts::from_appearance(appearance).apply_rgba(&sprite.rgba)
        } else {
            sprite.rgba.clone()
        };
        let color_img = ColorImage::from_rgba_unmultiplied(
            [sprite.width as usize, sprite.height as usize],
            &pixels,
        );
        let tag = if is_dream { "d" } else { "b" };
        let handle = ctx.load_texture(
            format!("{prefix}_{tag}_av_{image_idx}_{cache_color:?}"),
            color_img,
            TextureOptions::NEAREST,
        );
        let pair = (handle, is_one_by_one);
        // Bound GPU texture growth in long sessions with many uniquely colored avatars.
        if avatar_textures.len() >= MAX_AVATAR_TEXTURES {
            avatar_textures.clear();
        }
        avatar_textures.insert((is_dream, image_idx, cache_color), pair.clone());
        Some(pair)
    }

    /// Resolves, colorizes (via Furcadia `B == 0` green-channel LUTs only when `category & 0x20 != 0`),
    /// and caches avatar layers, supporting `[dream_archive, default_archive]` fallback and KitterSpeak
    /// frame animations. Returns `(layers, is_animated)` sorted shadow-first.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)] // Internal avatar renderer returning `(Vec<(tex, offset, category)>, is_animated)`.
    fn get_or_load_avatar_layers(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        appearance: &AvatarAppearance,
        obj_id: u16,
        gender: u8,
        dir: u8,
        pose: u8,
        anim_ms: u64,
    ) -> Option<(Vec<(TextureHandle, (i16, i16), u16)>, bool)> {
        let mut found: Option<(bool, FoxAvatarShape)> = None;
        let pass_flags: &[bool] = if obj_id == 0 {
            &[true]
        } else {
            &[false, true]
        };
        for &allow_obj0_fallback in pass_flags {
            for (is_dream, maybe_fox) in [
                (true, self.dream_archive.as_ref()),
                (false, self.default_archive.as_ref()),
            ] {
                let Some(fox) = maybe_fox else {
                    continue;
                };
                let eff_pose = if fox.object_pose_clamp(obj_id) && (1..=4).contains(&pose) {
                    2
                } else {
                    pose
                };
                if let Some(sh) = fox.resolve_avatar_shape_opt(
                    obj_id,
                    gender,
                    dir,
                    eff_pose,
                    allow_obj0_fallback,
                ) {
                    found = Some((is_dream, sh.clone()));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }

        let (is_dream, shape) = found?;
        let is_animated = !shape.anim_steps.is_empty();
        let fox = if is_dream {
            self.dream_archive.as_mut()?
        } else {
            self.default_archive.as_mut()?
        };

        let mut layer_batches: Vec<((i16, i16), Vec<FoxLayer>)> = Vec::new();
        if let Some(resolved) = shape.slots_at(anim_ms) {
            for active in resolved.slots.iter().flatten() {
                if let Some(frame) = shape.frames.get(active.frame_idx) {
                    if !frame.layers.is_empty() {
                        layer_batches.push((active.offset, frame.layers.clone()));
                    }
                }
            }
        }
        if layer_batches.is_empty() && !shape.layers.is_empty() {
            layer_batches.push((shape.offset, shape.layers.clone()));
        }

        let mut out = Vec::new();
        for (slot_off, mut layers) in layer_batches {
            // Draw shadows (category bit 0x40) underneath body/wings layers.
            layers.sort_by_key(|l| if (l.category & 0x40) != 0 { 0 } else { 1 });
            for layer in layers {
                let is_shadow = (layer.category & 0x40) != 0;
                let remap_color = !is_shadow && (layer.category & 0x20) != 0;
                let Some((tex, is_one_by_one)) = Self::decode_or_get_avatar_image(
                    fox,
                    is_dream,
                    &self.prefix,
                    &mut self.avatar_textures,
                    ctx,
                    palette,
                    appearance,
                    layer.image_idx,
                    remap_color,
                ) else {
                    continue;
                };
                if is_one_by_one {
                    continue;
                }
                let total_off = (
                    slot_off.0.saturating_add(layer.layer_offset.0),
                    slot_off.1.saturating_add(layer.layer_offset.1),
                );
                out.push((tex, total_off, layer.category));
            }
        }

        if out.is_empty() {
            None
        } else {
            Some((out, is_animated))
        }
    }

    /// Resolves a species portrait (`purpose = 4`) or gender/species icon (`purpose = 35`).
    fn get_or_load_portrait_tex(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        appearance: &AvatarAppearance,
        obj_id: u16,
        gender: u8,
        purpose: u8,
    ) -> Option<TextureHandle> {
        const GENDER_NIBBLE: [u8; 3] = [1, 2, 4];
        let nibble = GENDER_NIBBLE[(gender as usize).min(2)];
        let try_ids: &[u16] = if obj_id == 0 { &[0] } else { &[obj_id, 0] };

        for &try_id in try_ids {
            let archives = if purpose == 4 || purpose == 35 {
                // A dream patch may provide a non-remappable placeholder portrait.
                // Prefer the canonical species portrait so avatar colors can map.
                [
                    (false, self.default_archive.as_mut()),
                    (true, self.dream_archive.as_mut()),
                ]
            } else {
                [
                    (true, self.dream_archive.as_mut()),
                    (false, self.default_archive.as_mut()),
                ]
            };
            for (is_dream, maybe_fox) in archives {
                let Some(fox) = maybe_fox else {
                    continue;
                };
                let Some(shape) = fox
                    .resolve_object_shape_ps(try_id, purpose, Some(nibble), 0)
                    .cloned()
                else {
                    continue;
                };
                let layers = shape
                    .frames
                    .first()
                    .filter(|f| !f.layers.is_empty())
                    .map(|f| f.layers.as_slice())
                    .unwrap_or(shape.layers.as_slice());
                let candidate = layers
                    .iter()
                    .find(|l| (l.category & 0x40) == 0 && (l.category & 0x80) == 0)
                    .or_else(|| layers.iter().find(|l| (l.category & 0x80) == 0))
                    .or_else(|| layers.first());
                let Some(layer) = candidate else {
                    continue;
                };
                let remap_color = (layer.category & 0x40) == 0;
                if let Some((tex, is_one_by_one)) = Self::decode_or_get_avatar_image(
                    fox,
                    is_dream,
                    &self.prefix,
                    &mut self.avatar_textures,
                    ctx,
                    palette,
                    appearance,
                    layer.image_idx,
                    remap_color,
                ) {
                    if !is_one_by_one {
                        return Some(tex);
                    }
                }
            }
        }
        None
    }
}

/// Commands sent from the `egui` UI thread to a session's [`network_worker`] thread.
enum UiToNet {
    AuthenticateAccount {
        email: String,
        password: String,
    },
    ConnectCharacter {
        seq: u64,
        email: String,
        character: String,
        password: String,
        updated_desc: Option<String>,
        colors: Option<String>,
    },
    Send(ClientCommand),
    DownloadDream {
        url: String,
        map_name: String,
        checksum: Option<String>,
    },
    DownloadSpecies {
        file_id: usize,
        server_code: Option<char>,
        if_newer_than: Option<std::path::PathBuf>,
    },
    DownloadStockPatch {
        slot: StockPatchSlot,
    },
    DownloadPortrait {
        portrait_id: u32,
    },
    DownloadChatImage {
        url: String,
    },
    SpellCheck {
        seq: u64,
        text: String,
        custom_dict: std::collections::HashSet<String>,
    },
    Disconnect,
}

#[derive(Clone, Debug)]
struct MisspelledWord {
    word: String,
    start: usize,
    end: usize,
    suggestions: Vec<String>,
}

enum NetToUi {
    Status(String),
    AuthError(String),
    Authenticated(AccountInfo),
    News(Vec<NewsEntry>),
    ConnectStage {
        seq: u64,
        stage: String,
    },
    ConnectFailed {
        seq: u64,
        error: String,
    },
    Connected {
        seq: u64,
        connected: bool,
    },
    Message(ServerMessage),
    DreamDownloadProgress {
        map_name: String,
        received_bytes: u64,
        total_bytes: Option<u64>,
        stage: String,
    },
    DreamDownloaded {
        map_name: String,
        path: std::path::PathBuf,
        dream_dir: Option<std::path::PathBuf>,
    },
    DreamError(String),
    SpeciesDownloaded {
        file_id: usize,
        path: Option<std::path::PathBuf>,
    },
    StockPatchDownloaded {
        slot: StockPatchSlot,
        archive: Option<Box<FoxArchive>>,
    },
    PortraitDownloaded {
        portrait_id: u32,
        rgba: Option<(usize, usize, Vec<u8>)>,
        remappable: bool,
    },
    ChatImageDownloaded {
        url: String,
        rgba: Option<(usize, usize, Vec<u8>)>,
    },
    SpellCheckResult {
        seq: u64,
        misspelled: Vec<MisspelledWord>,
    },
}

#[derive(Debug, Clone, Copy)]
struct CacheUsageStats {
    species: (usize, u64),
    dreams: (usize, u64),
    patches: (usize, u64),
}

struct BackgroundSession {
    char_idx: usize,
    char_name: String,
    live_desc_input: String,
    is_connected: bool,
    status_text: String,
    world: WorldState,
    chat_log: Vec<ChatLine>,
    whispers: Vec<WhisperSession>,
    active_chat_tab: Option<String>,
    main_chat_draft: String,
    pending_whisper_echoes: Vec<(String, String, Instant)>,
    current_dream_dir: Option<std::path::PathBuf>,
    cam_offset: Vec2,
    unread_count: usize,
    cookies_collected: Option<u32>,
    cookies_total: Option<u32>,
    looked_player: Option<LookedPlayerCard>,
    main_looked_player: Option<LookedPlayerCard>,
    to_net: Sender<UiToNet>,
    from_net: Receiver<NetToUi>,
}

/// Root `eframe::App` state for the Furcadia GUI client.
///
/// Holds the active character session (network channels, [`WorldState`], chat/whisper
/// logs, Pounce monitor), any background multi-character sessions (`bg_sessions`),
/// GPU texture caches, and persisted user settings (`ClientConfig`).
#[allow(dead_code)]
struct FurcadiaApp {
    screen: AppScreen,

    email: String,
    password: String,
    remember_email: bool,
    remember_password: bool,
    auto_login: bool,
    auth_busy: bool,
    auth_error: Option<String>,

    account_info: Option<AccountInfo>,
    selected_char_idx: usize,
    char_sort: CharSort,
    editing_char: bool,
    edit_desc_buf: String,
    pending_desc: Option<String>,
    news: Vec<NewsEntry>,
    char_portraits: HashMap<String, Option<TextureHandle>>,

    status_text: String,
    is_connected: bool,
    connect_seq: u64,
    connecting_stage: String,
    connecting_error: Option<String>,
    connected_at: Option<Instant>,

    chat_input: String,
    main_chat_draft: String,
    spell_seq: u64,
    spell_dirty_at: Option<Instant>,
    spell_misspelled: Vec<MisspelledWord>,
    custom_dict_words: std::collections::HashSet<String>,
    chat_log: Vec<ChatLine>,
    main_chat_unread: usize,
    whispers: Vec<WhisperSession>,
    active_chat_tab: Option<String>,
    focus_chat_input: bool,
    dragging_whisper_tab: Option<usize>,
    active_char_tab_pos: usize,
    dragging_char_tab: Option<usize>,

    sidebar_tab: SidebarTab,
    theme: DreamTheme,
    dream_url_input: String,
    target_player_input: String,
    live_desc_input: String,
    afk_active: bool,
    show_debug_packets: bool,
    show_avatar_names: bool,
    import_path_input: String,
    import_source_detected: bool,

    watch_input: String,
    pounce: PounceMonitor,
    pounce_visible: bool,
    pounce_detached: bool,
    pounce_show_offline: bool,
    pounce_panel_width: f32,
    chat_panel_height: f32,

    assets: AssetLocator,
    palette: Palette,
    floors_cache: FoxSpriteCache,
    floors2_cache: FoxSpriteCache,
    floore_cache: FoxSpriteCache,
    items_cache: FoxSpriteCache,
    iteme_cache: FoxSpriteCache,
    portals_cache: FoxSpriteCache,
    portalse_cache: FoxSpriteCache,
    effects_cache: FoxSpriteCache,
    effectse_cache: FoxSpriteCache,
    lighting_cache: FoxSpriteCache,
    lightinge_cache: FoxSpriteCache,
    ambience_cache: FoxSpriteCache,
    ambiencee_cache: FoxSpriteCache,
    dream_item_splits: Vec<(u16, u16, u16, FoxSpriteCache)>,
    legacy_item_splits: Vec<(u16, u16, u16, FoxSpriteCache)>,
    junction_cache: FoxSpriteCache,
    wall_caches: HashMap<usize, FoxSpriteCache>,
    species_caches: HashMap<usize, FoxSpriteCache>,
    pending_species_downloads: std::collections::HashSet<usize>,
    pending_stock_patch_downloads: std::collections::HashSet<StockPatchSlot>,
    dynamic_avatars: HashMap<u16, (usize, bool)>,
    dynamic_avatar_server: Option<char>,
    custom_portrait_rgba: HashMap<u32, Option<(usize, usize, Vec<u8>)>>,
    custom_portrait_textures: HashMap<String, TextureHandle>,
    remappable_portraits: std::collections::HashSet<u32>,
    pending_portrait_downloads: std::collections::HashSet<u32>,
    player_portrait_ids: HashMap<String, u32>,
    smiley_textures: Vec<Option<TextureHandle>>,
    desctag_textures: Vec<Option<TextureHandle>>,
    chat_img_textures: HashMap<String, Option<TextureHandle>>,
    pending_chat_img_downloads: std::collections::HashSet<String>,
    player_badge_colors: HashMap<String, Color32>,
    current_dream_dir: Option<std::path::PathBuf>,
    current_dream_url: Option<String>,
    world: WorldState,
    pending_vascodagama: bool,
    login_ack_at: Option<Instant>,
    login_trace_remaining: usize,
    pending_dream_modern: Option<bool>,
    pending_dream_name: Option<String>,
    pending_dream_received_bytes: u64,
    pending_dream_total_bytes: Option<u64>,
    pending_dream_stage: String,
    looked_player: Option<LookedPlayerCard>,
    main_looked_player: Option<LookedPlayerCard>,
    known_player_cards: HashMap<String, LookedPlayerCard>,
    trade_item_input: String,
    trade_reason_input: String,
    pending_glook_target: Option<(String, Instant)>,
    pending_silent_tab_glooks: Vec<SilentTabGlook>,
    recent_portrait_target: Option<(String, Instant)>,
    silent_self_glook: bool,
    hovered_avatar_uid: Option<u32>,
    context_menu_avatar: Option<ContextMenuTarget>,

    cam_offset: Vec2,
    zoom: f32,

    to_net: Sender<UiToNet>,
    from_net: Receiver<NetToUi>,
    bg_sessions: Vec<BackgroundSession>,

    // Settings, logging, notifications, tray, fonts & importer state
    logging_enabled: bool,
    log_whispers: bool,
    shared_logs_mode: bool,
    log_retention_days: u32,
    notify_enabled: bool,
    notify_whisper: bool,
    notify_pounce: bool,
    notify_mention: bool,
    notify_only_unfocused: bool,
    flash_taskbar_on_message: bool,
    flash_taskbar_on_main_chat: bool,
    blink_tray_on_alert: bool,
    mention_nicknames: String,
    sound_enabled: bool,
    music_enabled: bool,
    minimize_to_tray: bool,
    close_to_tray: bool,
    tray: TrayHandle,
    is_wayland: bool,
    #[cfg(target_os = "linux")]
    x11_window_id: Option<u32>,
    hidden_in_tray: bool,
    was_minimized: bool,
    tray_restore_until: Option<Instant>,
    allow_quit: bool,
    close_confirm_open: bool,
    window_width: u16,
    window_height: u16,
    window_maximized: bool,
    window_geometry_dirty_at: Option<Instant>,
    font_family: String,
    font_size: u16,
    chat_font_size: u16,
    colors: UiColors,
    autocorrect: bool,
    fonts_configured: bool,
    has_ui_bold_font: bool,
    has_fixedsys_font: bool,
    applied_font_family: String,
    applied_font_size: u16,
    settings_open: bool,
    show_settings_tab: bool,
    sync_folder: String,
    sync_folder_input: String,
    sync_folder_message: Option<String>,
    sync_device_id: String,
    log_viewer_open: bool,
    log_viewer_char: String,
    log_viewer_files: Vec<std::path::PathBuf>,
    log_viewer_selected: Option<std::path::PathBuf>,
    log_viewer_content: String,
    smiley_picker_open: bool,
    link_builder_open: bool,
    link_url_input: String,
    link_label_input: String,
    last_visible_avatar_count: usize,
    music_child: Option<std::process::Child>,
    help_open: bool,
    about_open: bool,
    bear_logo: Option<TextureHandle>,
    chat_scroll_delta: f32,
    chat_pinned_to_bottom: bool,
    chat_scroll_to_bottom: bool,
    last_chat_avail_h: f32,
    reconnecting: bool,
    font_popup_open: bool,
    import_report: Option<String>,
    cache_report: Option<String>,
    /// Cached download file/byte counts, computed on demand for Settings.
    cache_usage: Option<CacheUsageStats>,
    /// Explicit download URL used for the current dream (player dreams), so it can be re-fetched.
    last_dream_explicit_url: Option<String>,
    /// Last dream identifier passed to `enter_dream`, used to re-download after flushing dream cache.
    last_dream_id: Option<String>,
    window_focused: bool,
    pending_taskbar_flash: bool,
    taskbar_flash_active: bool,
    posture: u8,
    anim_mode: String,
    last_sent_msg: String,
    last_whisper_target: String,
    last_received_whisper_from: String,
    pending_whisper_echoes: Vec<(String, String, Instant)>,
    saved_character: String,
    whisper_open_tab_on_incoming: bool,
    whisper_emote_in_whisper: bool,
    last_dice_roll_at: Option<Instant>,
    settings_tab: usize,
    dream_bookmarks: Vec<String>,
    limit_view_range: bool,
    allow_dream_skins: bool,
    portrait_size: f32,
    custom_dream_skin: Option<(Color32, Color32, Color32)>,
    check_updates_on_startup: bool,
    first_setup_done: bool,
    first_setup_open: bool,
    update_checking: bool,
    update_status: Option<String>,
    update_available: Option<UpdateReleaseInfo>,
    update_popup_open: bool,
    update_rx: Option<mpsc::Receiver<UpdateCheckMsg>>,
    update_install_state: UpdateInstallState,
    update_install_rx: Option<mpsc::Receiver<UpdateInstallMsg>>,
    cookies_collected: Option<u32>,
    cookies_total: Option<u32>,
    dice_roll: String,
}

impl FurcadiaApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (to_net, net_rx) = mpsc::channel::<UiToNet>();
        let (net_tx, from_net) = mpsc::channel::<NetToUi>();
        let ctx = cc.egui_ctx.clone();
        let net_ctx = ctx.clone();

        thread::spawn(move || network_worker(net_rx, net_tx, net_ctx));

        let cfg = ClientConfig::load_default();
        AssetLocator::purge_mislabeled_species_cache();
        let assets = AssetLocator::discover();
        let discovered_import_dirs = discover_default_windows_furcadia_dirs();
        let import_source_detected = !discovered_import_dirs.is_empty();
        let import_path_input = discovered_import_dirs
            .first()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Furcadia Windows/program Files (x86)/Furcadia".to_owned());
        let palette = assets.load_default_palette().unwrap_or_default();
        let floors_cache =
            FoxSpriteCache::load_with_legacy(&assets, "floors.fox", Some("floors.fox"), "floor");
        let floors2_cache = FoxSpriteCache::load_legacy_only(&assets, "floors2.fox", "floors2");
        let floore_cache =
            FoxSpriteCache::load_with_legacy(&assets, "floore.fox", Some("floore.fox"), "floore");
        let items_cache =
            FoxSpriteCache::load_with_legacy(&assets, "items.fox", Some("item.fox"), "item");
        let iteme_cache =
            FoxSpriteCache::load_with_legacy(&assets, "iteme.fox", Some("iteme.fox"), "iteme");
        let portals_cache = FoxSpriteCache::load(&assets, "portals.fox", "portal");
        let portalse_cache = FoxSpriteCache::load(&assets, "portalse.fox", "portalse");
        let effects_cache = FoxSpriteCache::load_with_legacy(
            &assets,
            "effects.fox",
            Some("effects.fox"),
            "effect",
        );
        let effectse_cache = FoxSpriteCache::load_with_legacy(
            &assets,
            "effectse.fox",
            Some("effectse.fox"),
            "effectse",
        );
        let lighting_cache = FoxSpriteCache::load(&assets, "lighting.fox", "lighting");
        let lightinge_cache = FoxSpriteCache::load(&assets, "lightinge.fox", "lightinge");
        let ambience_cache = FoxSpriteCache::load(&assets, "ambience.fox", "ambience");
        let ambiencee_cache = FoxSpriteCache::load(&assets, "ambiencee.fox", "ambiencee");
        let junction_cache = FoxSpriteCache::load_with_legacy_candidates(
            &assets,
            "junction.fox",
            &["junction.fsh", "junction.fox"],
            "junction",
        );
        let mut legacy_item_splits = Vec::new();
        for (fname, min_id, max_id, offset) in [
            ("item2.fox", 305u16, 999u16, 305u16),
            ("item3.fox", 1000, 1348, 1000),
            ("item4.fox", 1349, 1748, 1349),
        ] {
            let cache = FoxSpriteCache::load_legacy_only(&assets, fname, fname);
            legacy_item_splits.push((min_id, max_id, offset, cache));
        }
        let mut wall_caches = HashMap::new();
        wall_caches.insert(
            0,
            FoxSpriteCache::load_with_legacy(&assets, "wall.fox", Some("wall.fox"), "wall_0"),
        );
        for g in 1..=20 {
            let fname = format!("wall_{g}.fox");
            if assets.default_patch_path(&fname).is_some()
                || assets.legacy_patch_path(&fname).is_some()
            {
                wall_caches.insert(
                    g,
                    FoxSpriteCache::load_with_legacy(
                        &assets,
                        &fname,
                        Some(&fname),
                        &format!("wall_{g}"),
                    ),
                );
            }
        }
        let mut smiley_textures: Vec<Option<TextureHandle>> = Vec::with_capacity(54);
        for (fsh_name, slot_prefix) in [("smileys.fsh", "sm0"), ("smileys2.fsh", "sm1")] {
            if let Some(raw) = assets
                .default_patch_path(fsh_name)
                .and_then(|p| std::fs::read(p).ok())
            {
                for (i, sp) in decode_fsh_sprites(&raw, &palette).into_iter().enumerate() {
                    if sp.width == 0 || sp.height == 0 {
                        smiley_textures.push(None);
                        continue;
                    }
                    let img = ColorImage::from_rgba_unmultiplied(
                        [sp.width as usize, sp.height as usize],
                        &sp.rgba,
                    );
                    let tex = ctx.load_texture(
                        format!("{slot_prefix}_{i}"),
                        img,
                        TextureOptions::NEAREST,
                    );
                    smiley_textures.push(Some(tex));
                }
            }
        }

        let mut desctag_textures: Vec<Option<TextureHandle>> = Vec::new();
        if let Some(p) = assets.default_patch_path("desctags.fox") {
            if let Ok(mut archive) = FoxArchive::load(&p) {
                for idx in 0..archive.images.len() {
                    if let Ok(sp) = archive.decode_image(idx, &palette) {
                        if sp.width > 0 && sp.height > 0 {
                            let img = ColorImage::from_rgba_unmultiplied(
                                [sp.width as usize, sp.height as usize],
                                &sp.rgba,
                            );
                            let tex = ctx.load_texture(
                                format!("desctag_{idx}"),
                                img,
                                TextureOptions::NEAREST,
                            );
                            desctag_textures.push(Some(tex));
                            continue;
                        }
                    }
                    desctag_textures.push(None);
                }
            }
        }

        let mut pounce = PounceMonitor::default();
        for w in &cfg.watchlist {
            pounce.add_watch(w);
        }

        let world = WorldState::default();

        let mut auth_busy = false;
        if cfg.auto_login && !cfg.email.is_empty() && !cfg.password.is_empty() {
            auth_busy = true;
            let _ = to_net.send(UiToNet::AuthenticateAccount {
                email: cfg.email.clone(),
                password: cfg.password.clone(),
            });
        }

        if cfg.log_retention_days > 0 {
            if let Some(dir) = default_logs_dir() {
                let _ = prune_old_logs(&dir, cfg.log_retention_days);
            }
        }

        let colors = UiColors::from_config(&cfg);
        let mut app = Self {
            screen: AppScreen::Login,
            email: cfg.email,
            password: cfg.password,
            remember_email: cfg.remember_email,
            remember_password: cfg.remember_password,
            auto_login: cfg.auto_login,
            auth_busy,
            auth_error: None,
            account_info: None,
            selected_char_idx: 0,
            char_sort: CharSort::Recent,
            editing_char: false,
            edit_desc_buf: String::new(),
            pending_desc: None,
            news: Vec::new(),
            char_portraits: HashMap::new(),
            status_text: "Disconnected".to_owned(),
            is_connected: false,
            connect_seq: 0,
            connecting_stage: String::new(),
            connecting_error: None,
            connected_at: None,
            chat_input: String::new(),
            main_chat_draft: String::new(),
            spell_seq: 0,
            spell_dirty_at: None,
            spell_misspelled: Vec::new(),
            custom_dict_words: load_custom_dictionary(),
            chat_log: vec![parse_chat_line(
                "<font color='success'>Welcome to the Native Linux Furcadia Client.</font>",
            )],
            main_chat_unread: 0,
            whispers: Vec::new(),
            active_chat_tab: None,
            focus_chat_input: false,
            dragging_whisper_tab: None,
            active_char_tab_pos: 0,
            dragging_char_tab: None,
            sidebar_tab: SidebarTab::Move,
            theme: match cfg.theme.as_str() {
                "DragonHoard" => DreamTheme::DragonHoard,
                "KitterMagic" => DreamTheme::KitterMagic,
                "FerianWylde" => DreamTheme::FerianWylde,
                _ => DreamTheme::ClassicPurple,
            },
            dream_url_input: "furc://".to_owned(),
            target_player_input: String::new(),
            live_desc_input: String::new(),
            afk_active: false,
            show_debug_packets: cfg.show_debug_packets,
            show_avatar_names: cfg.show_avatar_names,
            import_path_input,
            import_source_detected,
            watch_input: String::new(),
            pounce,
            pounce_visible: cfg.pounce_visible,
            pounce_detached: cfg.pounce_detached,
            pounce_show_offline: cfg.pounce_show_offline,
            pounce_panel_width: 260.0,
            chat_panel_height: 260.0,
            assets,
            palette,
            floors_cache,
            floors2_cache,
            floore_cache,
            items_cache,
            iteme_cache,
            portals_cache,
            portalse_cache,
            effects_cache,
            effectse_cache,
            lighting_cache,
            lightinge_cache,
            ambience_cache,
            ambiencee_cache,
            dream_item_splits: Vec::new(),
            legacy_item_splits,
            junction_cache,
            wall_caches,
            species_caches: HashMap::new(),
            pending_species_downloads: std::collections::HashSet::new(),
            pending_stock_patch_downloads: std::collections::HashSet::new(),
            dynamic_avatars: HashMap::new(),
            dynamic_avatar_server: None,
            custom_portrait_rgba: HashMap::new(),
            custom_portrait_textures: HashMap::new(),
            remappable_portraits: std::collections::HashSet::new(),
            pending_portrait_downloads: std::collections::HashSet::new(),
            player_portrait_ids: cfg
                .character_portraits
                .iter()
                .map(|(k, &v)| (k.clone(), v))
                .collect(),
            smiley_textures,
            desctag_textures,
            chat_img_textures: HashMap::new(),
            pending_chat_img_downloads: std::collections::HashSet::new(),
            player_badge_colors: HashMap::new(),
            current_dream_dir: None,
            current_dream_url: None,
            world,
            pending_vascodagama: false,
            login_ack_at: None,
            login_trace_remaining: 0,
            pending_dream_modern: None,
            pending_dream_name: None,
            pending_dream_received_bytes: 0,
            pending_dream_total_bytes: None,
            pending_dream_stage: String::new(),
            looked_player: None,
            main_looked_player: None,
            known_player_cards: HashMap::new(),
            trade_item_input: String::new(),
            trade_reason_input: String::new(),
            pending_glook_target: None,
            pending_silent_tab_glooks: Vec::new(),
            recent_portrait_target: None,
            silent_self_glook: false,
            hovered_avatar_uid: None,
            context_menu_avatar: None,
            cam_offset: Vec2::ZERO,
            zoom: if cfg.limit_view_range { 1.0 } else { 1.25 },
            to_net,
            from_net,
            bg_sessions: Vec::new(),
            logging_enabled: cfg.logging_enabled,
            log_whispers: cfg.log_whispers,
            shared_logs_mode: cfg.shared_logs_mode,
            log_retention_days: cfg.log_retention_days,
            notify_enabled: cfg.notify_enabled,
            notify_whisper: cfg.notify_whisper,
            notify_pounce: cfg.notify_pounce,
            notify_mention: cfg.notify_mention,
            notify_only_unfocused: cfg.notify_only_unfocused,
            flash_taskbar_on_message: cfg.flash_taskbar_on_message,
            flash_taskbar_on_main_chat: cfg.flash_taskbar_on_main_chat,
            blink_tray_on_alert: cfg.blink_tray_on_alert,
            mention_nicknames: cfg.mention_nicknames.clone(),
            sound_enabled: cfg.sound_enabled,
            music_enabled: cfg.music_enabled,
            minimize_to_tray: cfg.minimize_to_tray,
            close_to_tray: cfg.close_to_tray,
            tray: {
                let tray_ctx = cc.egui_ctx.clone();
                spawn_tray(
                    include_bytes!("../../packaging/linux/furcadia-native.png"),
                    move || tray_ctx.request_repaint(),
                )
            },
            is_wayland: detect_window_backend(cc).0,
            #[cfg(target_os = "linux")]
            x11_window_id: detect_window_backend(cc).1,
            hidden_in_tray: false,
            was_minimized: false,
            tray_restore_until: None,
            allow_quit: false,
            close_confirm_open: false,
            window_width: cfg.window_width,
            window_height: cfg.window_height,
            window_maximized: cfg.window_maximized,
            window_geometry_dirty_at: None,
            font_family: cfg.font_family.clone(),
            font_size: cfg.font_size,
            chat_font_size: cfg.chat_font_size,
            colors,
            autocorrect: cfg.autocorrect,
            fonts_configured: false,
            has_ui_bold_font: false,
            has_fixedsys_font: false,
            applied_font_family: String::new(),
            applied_font_size: 0,
            settings_open: false,
            show_settings_tab: false,
            sync_folder: cfg.sync_folder.clone(),
            sync_folder_input: cfg.sync_folder.clone(),
            sync_folder_message: None,
            sync_device_id: cfg.sync_device_id.clone(),
            log_viewer_open: false,
            log_viewer_char: String::new(),
            log_viewer_files: Vec::new(),
            log_viewer_selected: None,
            log_viewer_content: String::new(),
            smiley_picker_open: false,
            link_builder_open: false,
            link_url_input: String::new(),
            link_label_input: String::new(),
            last_visible_avatar_count: 0,
            music_child: None,
            help_open: false,
            about_open: false,
            bear_logo: None,
            chat_scroll_delta: 0.0,
            chat_pinned_to_bottom: true,
            chat_scroll_to_bottom: true,
            last_chat_avail_h: 0.0,
            reconnecting: false,
            font_popup_open: false,
            import_report: None,
            cache_report: None,
            cache_usage: None,
            last_dream_explicit_url: None,
            last_dream_id: None,
            window_focused: true,
            pending_taskbar_flash: false,
            taskbar_flash_active: false,
            posture: 0,
            anim_mode: cfg.anim_mode.clone(),
            last_sent_msg: String::new(),
            last_whisper_target: String::new(),
            last_received_whisper_from: String::new(),
            pending_whisper_echoes: Vec::new(),
            saved_character: cfg.character.clone(),
            whisper_open_tab_on_incoming: cfg.whisper_open_tab_on_incoming,
            whisper_emote_in_whisper: cfg.whisper_emote_in_whisper,
            last_dice_roll_at: None,
            settings_tab: 0,
            dream_bookmarks: cfg.dream_bookmarks.clone(),
            limit_view_range: cfg.limit_view_range,
            allow_dream_skins: cfg.allow_dream_skins,
            portrait_size: cfg.portrait_size as f32,
            custom_dream_skin: None,
            check_updates_on_startup: cfg.check_updates_on_startup,
            first_setup_done: cfg.first_setup_done,
            first_setup_open: !cfg.first_setup_done,
            update_checking: false,
            update_status: None,
            update_available: None,
            update_popup_open: false,
            update_rx: None,
            update_install_state: UpdateInstallState::Idle,
            update_install_rx: None,
            cookies_collected: None,
            cookies_total: None,
            dice_roll: cfg.dice_roll.clone(),
        };
        app.apply_font_settings(&ctx);
        if app.first_setup_done && app.check_updates_on_startup {
            app.trigger_update_check(&ctx);
        }
        app
    }

    fn selected_character(&self) -> Option<&CharacterSummary> {
        let info = self.account_info.as_ref()?;
        info.characters.get(self.selected_char_idx)
    }

    fn save_config(&self) {
        let _ = self.current_config().save_default();
    }

    fn current_config(&self) -> ClientConfig {
        let character = self
            .selected_character()
            .map(|c| c.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.saved_character.clone());
        let theme_name = match self.theme {
            DreamTheme::ClassicPurple => "ClassicPurple",
            DreamTheme::DragonHoard => "DragonHoard",
            DreamTheme::KitterMagic => "KitterMagic",
            DreamTheme::FerianWylde => "FerianWylde",
        };
        let mut character_portraits = std::collections::BTreeMap::new();
        if let Some(info) = self.account_info.as_ref() {
            for ch in &info.characters {
                let key = shortname_key(&ch.name);
                if let Some(&pid) = self.player_portrait_ids.get(&key) {
                    if !key.is_empty() && pid > 0 {
                        character_portraits.insert(key, pid);
                    }
                }
            }
        }
        for (name, &pid) in &self.player_portrait_ids {
            if character_portraits.len() >= 128 {
                break;
            }
            let key = shortname_key(name);
            if !key.is_empty() && pid > 0 {
                character_portraits.entry(key).or_insert(pid);
            }
        }
        let mut cfg = ClientConfig {
            email: self.email.clone(),
            character,
            password: self.password.clone(),
            remember_email: self.remember_email,
            remember_password: self.remember_password,
            auto_login: self.auto_login,
            watchlist: self.pounce.entries().map(|e| e.name.clone()).collect(),
            logging_enabled: self.logging_enabled,
            log_whispers: self.log_whispers,
            shared_logs_mode: self.shared_logs_mode,
            log_retention_days: self.log_retention_days,
            notify_enabled: self.notify_enabled,
            notify_whisper: self.notify_whisper,
            notify_pounce: self.notify_pounce,
            notify_mention: self.notify_mention,
            notify_only_unfocused: self.notify_only_unfocused,
            flash_taskbar_on_message: self.flash_taskbar_on_message,
            flash_taskbar_on_main_chat: self.flash_taskbar_on_main_chat,
            blink_tray_on_alert: self.blink_tray_on_alert,
            mention_nicknames: self.mention_nicknames.clone(),
            sound_enabled: self.sound_enabled,
            music_enabled: self.music_enabled,
            minimize_to_tray: self.minimize_to_tray,
            close_to_tray: self.close_to_tray,
            font_family: self.font_family.clone(),
            font_size: self.font_size,
            chat_font_size: self.chat_font_size,
            autocorrect: self.autocorrect,
            anim_mode: self.anim_mode.clone(),
            theme: theme_name.to_owned(),
            show_avatar_names: self.show_avatar_names,
            pounce_visible: self.pounce_visible,
            pounce_detached: self.pounce_detached,
            pounce_show_offline: self.pounce_show_offline,
            show_debug_packets: self.show_debug_packets,
            whisper_open_tab_on_incoming: self.whisper_open_tab_on_incoming,
            whisper_emote_in_whisper: self.whisper_emote_in_whisper,
            dream_bookmarks: self.dream_bookmarks.clone(),
            limit_view_range: self.limit_view_range,
            allow_dream_skins: self.allow_dream_skins,
            portrait_size: self.portrait_size.round().clamp(48.0, 160.0) as u16,
            check_updates_on_startup: self.check_updates_on_startup,
            first_setup_done: self.first_setup_done,
            dice_roll: if self.dice_roll.trim().is_empty() {
                "1d6".to_owned()
            } else {
                self.dice_roll.trim().to_owned()
            },
            sync_folder: self.sync_folder.clone(),
            sync_device_id: self.sync_device_id.clone(),
            sync_updated_at: 0,
            window_width: self.window_width,
            window_height: self.window_height,
            window_maximized: self.window_maximized,
            character_portraits,
            ..ClientConfig::default()
        };
        self.colors.write_to_config(&mut cfg);
        cfg
    }

#[allow(dead_code)]
    fn apply_sync_folder(&mut self) {
        let requested = self.sync_folder_input.trim().to_owned();
        let old_logs = default_logs_dir();
        let mut cfg = self.current_config();

        if requested.is_empty() {
            cfg.sync_folder.clear();
            if let Err(err) = cfg.save_local_default() {
                self.sync_folder_message = Some(format!("Could not save settings: {err}"));
            } else {
                self.sync_folder.clear();
                self.sync_folder_input.clear();
                self.sync_folder_message = Some("Folder sync is turned off.".to_owned());
                if let (Some(source), Some(destination)) = (old_logs, default_logs_dir()) {
                    if source != destination {
                        if let Err(err) =
                            furcadia_client::logs::copy_logs_if_missing(&source, &destination)
                        {
                            self.sync_folder_message = Some(format!(
                                "Sync is off, but some cloud logs could not be copied locally: {err}"
                            ));
                        }
                    }
                }
            }
            return;
        }

        let requested_folder = expand_user_path(&requested);
        if let Err(err) = std::fs::create_dir_all(&requested_folder) {
            self.sync_folder_message = Some(format!("Could not open that folder: {err}"));
            return;
        }
        let folder = match std::fs::canonicalize(&requested_folder) {
            Ok(path) => path,
            Err(err) => {
                self.sync_folder_message = Some(format!("Could not resolve that folder: {err}"));
                return;
            }
        };
        if let Some(old) = old_logs.as_ref() {
            let same_as_active = self.sync_folder_path() == Some(folder.clone());
            if !same_as_active && (folder.starts_with(old) || old.starts_with(&folder)) {
                self.sync_folder_message = Some(
                    "Choose a dedicated cloud folder outside Furcadia's existing data folders."
                        .to_owned(),
                );
                return;
            }
        }

        cfg.sync_folder = folder.to_string_lossy().into_owned();
        let has_shared_settings = cfg.has_shared_settings();
        if let Err(err) = cfg.save_local_default() {
            self.sync_folder_message = Some(format!("Could not save sync folder: {err}"));
            return;
        }
        self.sync_folder = cfg.sync_folder.clone();
        self.sync_folder_input = cfg.sync_folder.clone();

        let mut message = if has_shared_settings {
            "Connected. Restart Furcadia Native Client to load the newest synced settings."
                .to_owned()
        } else {
            match cfg.save_default() {
                Ok(()) => "Sync folder initialized with this computer's settings.".to_owned(),
                Err(err) => format!("Folder selected, but settings could not be synced: {err}"),
            }
        };

        if let (Some(source), Some(destination)) = (old_logs, default_logs_dir()) {
            if source != destination {
                match furcadia_client::logs::copy_logs_if_missing(&source, &destination) {
                    Ok(count) if count > 0 => {
                        message.push_str(&format!(" Copied {count} existing log files."));
                    }
                    Ok(_) => {}
                    Err(err) => message.push_str(&format!(" Existing logs were not copied: {err}.")),
                }
            }
        }
        message.push_str(" Each device writes to its own logs folder; login credentials stay local.");
        self.sync_folder_message = Some(message);
    }

    fn sync_folder_path(&self) -> Option<std::path::PathBuf> {
        let value = self.sync_folder.trim();
        (!value.is_empty()).then(|| std::path::PathBuf::from(value))
    }

#[allow(dead_code)]
    fn browse_sync_folder(&mut self) {
        #[cfg(target_os = "windows")]
        let out = furcadia_client::sys::command("powershell")
            .args([
                "-NoProfile",
                "-STA",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.FolderBrowserDialog; $d.Description = 'Select a cloud-synced Furcadia folder'; if ($d.ShowDialog() -eq 'OK') { Write-Output $d.SelectedPath }",
            ])
            .output();
        #[cfg(target_os = "macos")]
        let out = std::process::Command::new("osascript")
            .args([
                "-e",
                "POSIX path of (choose folder with prompt \"Select a cloud-synced Furcadia folder\")",
            ])
            .output();
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let out = furcadia_client::sys::command("zenity")
            .args(["--file-selection", "--directory", "--title=Select a cloud-synced Furcadia folder"])
            .output()
            .or_else(|_| {
                furcadia_client::sys::command("kdialog")
                    .args(["--getexistingdirectory", "."])
                    .output()
            });
        if let Ok(output) = out {
            if output.status.success() {
                let picked = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !picked.is_empty() {
                    self.sync_folder_input = picked;
                }
            }
        }
    }

    fn find_extra_asset(&self, rel: &str) -> Option<std::path::PathBuf> {
        let mut candidates = Vec::new();
        if let Some(dir) = AssetLocator::user_assets_dir() {
            candidates.push(dir.join(rel));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join(rel));
            }
        }
        if let Some(root) = &self.assets.root {
            candidates.push(root.join(rel));
        }
        candidates.push(std::path::PathBuf::from("Furcadia Windows/program Files (x86)/Furcadia").join(rel));
        candidates.push(std::path::PathBuf::from(self.import_path_input.trim()).join(rel));
        candidates.into_iter().find(|p| p.is_file())
    }

    fn ui_font_family(&self, bold: bool) -> egui::FontFamily {
        if bold && self.has_ui_bold_font && self.font_family == "Proportional" {
            return egui::FontFamily::Name(FONT_FAMILY_UI_BOLD.into());
        }
        match self.font_family.as_str() {
            "Monospace" => egui::FontFamily::Monospace,
            "Fixedsys" if self.has_fixedsys_font => egui::FontFamily::Name("fixedsys".into()),
            "Fixedsys" => egui::FontFamily::Monospace,
            _ => egui::FontFamily::Proportional,
        }
    }

    fn apply_font_settings(&mut self, ctx: &egui::Context) {
        if !self.fonts_configured {
            self.fonts_configured = true;
            let (regular, bold) = discover_system_ui_fonts();
            let fixedsys = self
                .find_extra_asset("fsex2p00_public.ttf")
                .and_then(|p| load_valid_font_file(&p));
            let (defs, has_bold) = build_font_definitions(regular, bold, fixedsys);
            self.has_ui_bold_font = has_bold;
            self.has_fixedsys_font = defs
                .families
                .contains_key(&egui::FontFamily::Name("fixedsys".into()));
            ctx.set_fonts(defs);
        }
        if self.applied_font_family == self.font_family && self.applied_font_size == self.font_size {
            return;
        }
        self.applied_font_family = self.font_family.clone();
        self.applied_font_size = self.font_size;

        let family = self.ui_font_family(false);
        let heading_family = self.ui_font_family(true);
        let sz = (self.font_size as f32).clamp(10.0, 28.0).round();
        configure_text_styles(ctx, sz, family, heading_family);
    }

    fn play_sound(&self, wav_name: &str) {
        if !self.sound_enabled {
            return;
        }
        let rel = format!("online/{wav_name}");
        let path_opt = self
            .current_dream_dir
            .as_deref()
            .and_then(|d| Self::find_file_case_insensitive(d, wav_name))
            .or_else(|| self.find_extra_asset(&rel))
            .or_else(|| self.find_extra_asset(wav_name));
        let Some(path) = path_opt else {
            return;
        };
        thread::spawn(move || {
            #[cfg(target_os = "windows")]
            {
                let script = format!(
                    "(New-Object System.Media.SoundPlayer '{}').PlaySync()",
                    path.display().to_string().replace('\'', "''")
                );
                let _ = furcadia_client::sys::command("powershell")
                    .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("afplay")
                    .arg(&path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            for (bin, args) in [
                ("pw-play", Vec::<&str>::new()),
                ("paplay", vec![]),
                ("aplay", vec![]),
                ("ffplay", vec!["-nodisp", "-autoexit", "-loglevel", "quiet"]),
                ("mpv", vec!["--no-video", "--really-quiet"]),
            ] {
                if std::process::Command::new(bin)
                    .args(&args)
                    .arg(&path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
                {
                    break;
                }
            }
        });
    }

    fn stop_music(&mut self) {
        if let Some(mut child) = self.music_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn play_dream_music(&mut self, dir: &std::path::Path) {
        self.stop_music();
        if !self.music_enabled {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut music_file: Option<(std::path::PathBuf, bool)> = None;
        for ent in entries.flatten() {
            let p = ent.path();
            if !p.is_file() {
                continue;
            }
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            if matches!(ext.as_str(), "mid" | "midi") {
                music_file = Some((p, true));
                break;
            } else if matches!(ext.as_str(), "ogg" | "mp3" | "wav") {
                music_file = Some((p, false));
                break;
            }
        }
        let Some((path, is_midi)) = music_file else {
            return;
        };
        let candidates: Vec<(&str, Vec<&str>)> = if is_midi {
            vec![
                ("timidity", vec!["-id", "-Ol"]),
                ("fluidsynth", vec!["-i", "-n", "-a", "pulseaudio"]),
            ]
        } else {
            vec![
                ("ffplay", vec!["-nodisp", "-autoexit", "-loop", "0", "-loglevel", "quiet"]),
                ("mpv", vec!["--no-video", "--loop=inf", "--really-quiet"]),
                ("pw-play", vec![]),
            ]
        };
        for (bin, args) in candidates {
            if let Ok(child) = std::process::Command::new(bin)
                .args(&args)
                .arg(&path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                self.music_child = Some(child);
                break;
            }
        }
    }

    fn ui_bg(&self) -> Color32 {
        if self.colors.custom_ui_bg {
            self.colors.chat_bg
        } else {
            self.theme.bg()
        }
    }

    fn ui_panel(&self) -> Color32 {
        if self.colors.custom_ui_bg {
            self.colors.panel_bg
        } else {
            self.theme.panel()
        }
    }

    fn ui_accent(&self) -> Color32 {
        if self.colors.custom_ui_bg {
            self.colors.accent
        } else {
            self.theme.accent()
        }
    }

    fn effective_colors(&self) -> (Color32, Color32, Color32) {
        if self.colors.custom_ui_bg {
            return (self.colors.chat_bg, self.colors.panel_bg, self.colors.accent);
        }
        if self.allow_dream_skins {
            if let Some((bg, panel, accent)) = self.custom_dream_skin {
                return (bg, panel, accent);
            }
        }
        (self.theme.bg(), self.theme.panel(), self.theme.accent())
    }

    fn parse_skin_color(raw: &str) -> Option<Color32> {
        let s = raw.trim().trim_start_matches('#');
        if s.len() == 6 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            return Some(Color32::from_rgb(r, g, b));
        }
        let parts: Vec<u8> = s
            .split([',', ' '])
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse::<u8>().ok())
            .collect();
        if parts.len() >= 3 {
            return Some(Color32::from_rgb(parts[0], parts[1], parts[2]));
        }
        None
    }

    fn parse_dream_skin_ini(dir: &std::path::Path) -> Option<(Color32, Color32, Color32)> {
        let path = Self::find_file_case_insensitive(dir, "skin.ini")
            .or_else(|| Self::find_file_case_insensitive(dir, "furcadia.ini"))?;
        let text = std::fs::read_to_string(path).ok()?;
        let mut bg = None;
        let mut panel = None;
        let mut accent = None;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with([';', '#', '[']) {
                continue;
            }
            if let Some((k, v)) = trimmed.split_once('=') {
                let key = k.trim().to_ascii_lowercase();
                if let Some(c) = Self::parse_skin_color(v) {
                    if key == "bg" || key == "background" || key == "bgcolor" {
                        bg = Some(c);
                    } else if key == "panel" || key == "panelcolor" || key == "frame" {
                        panel = Some(c);
                    } else if key == "accent" || key == "border" || key == "highlight" {
                        accent = Some(c);
                    }
                }
            }
        }
        if bg.is_some() || panel.is_some() || accent.is_some() {
            Some((
                bg.unwrap_or(Color32::from_rgb(24, 18, 42)),
                panel.unwrap_or(Color32::from_rgb(36, 26, 62)),
                accent.unwrap_or(GOLD),
            ))
        } else {
            None
        }
    }

    fn send_desktop_notification(&self, title: &str, body: &str) {
        if self.blink_tray_on_alert && (!self.window_focused || self.hidden_in_tray) {
            self.tray.set_attention(true);
        }
        if !self.notify_enabled || (self.notify_only_unfocused && self.window_focused) {
            return;
        }
        let title = title.to_owned();
        let body = body.to_owned();
        thread::spawn(move || {
            #[cfg(target_os = "windows")]
            {
                let esc = |s: &str| s.replace('\'', "''");
                let script = format!(
                    "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; \
                     $n = New-Object System.Windows.Forms.NotifyIcon; \
                     $n.Icon = [System.Drawing.SystemIcons]::Information; $n.Visible = $true; \
                     $n.ShowBalloonTip(5000, '{}', '{}', [System.Windows.Forms.ToolTipIcon]::None); \
                     Start-Sleep -Seconds 6; $n.Dispose()",
                    esc(&title),
                    esc(&body)
                );
                let _ = furcadia_client::sys::command("powershell")
                    .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("osascript")
                .args([
                    "-e",
                    "on run argv",
                    "-e",
                    "display notification (item 2 of argv) with title (item 1 of argv)",
                    "-e",
                    "end run",
                    "--",
                    &title,
                    &body,
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            let _ = std::process::Command::new("notify-send")
                .args(["-a", "Furcadia", &title, &body])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        });
    }

    fn request_taskbar_flash(&mut self) {
        if !self.window_focused || self.hidden_in_tray {
            self.pending_taskbar_flash = true;
            if self.blink_tray_on_alert {
                self.tray.set_attention(true);
            }
        }
    }

    fn flush_taskbar_flash(&mut self, ctx: &egui::Context) {
        if self.window_focused && !self.hidden_in_tray {
            if self.taskbar_flash_active {
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Reset,
                ));
                self.taskbar_flash_active = false;
            }
            self.pending_taskbar_flash = false;
            self.tray.set_attention(false);
        } else if self.pending_taskbar_flash {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
            self.pending_taskbar_flash = false;
            self.taskbar_flash_active = true;
        }
    }

    fn active_character_name(&self) -> String {
        self.active_character_wire_name().replace('|', " ")
    }

    fn active_character_wire_name(&self) -> String {
        self.world
            .self_name
            .as_deref()
            .or_else(|| self.selected_character().map(|c| c.name.as_str()))
            .or_else(|| {
                (!self.saved_character.trim().is_empty()).then_some(self.saved_character.as_str())
            })
            .unwrap_or("Unknown")
            .to_owned()
    }

    fn active_tab_is_character(&self) -> bool {
        matches!(self.screen, AppScreen::InGame | AppScreen::Connecting)
    }

    fn open_character_tab_names(&self) -> (Option<String>, Vec<String>) {
        let bg_names: Vec<&str> = self
            .bg_sessions
            .iter()
            .map(|s| s.char_name.as_str())
            .collect();
        if self.active_tab_is_character() {
            let active = self.active_character_name();
            let all =
                ordered_character_tab_names(&bg_names, Some(&active), self.active_char_tab_pos);
            (Some(active), all)
        } else {
            (None, ordered_character_tab_names(&bg_names, None, 0))
        }
    }

    fn has_multiple_character_tabs(&self) -> bool {
        if self.active_tab_is_character() {
            !self.bg_sessions.is_empty()
        } else {
            self.bg_sessions.len() >= 2
        }
    }

    fn close_active_character_tab(&mut self, ctx: &egui::Context) {
        let active_pos = self.active_char_tab_pos.min(self.bg_sessions.len());
        self.connect_seq = self.connect_seq.wrapping_add(1);
        self.connected_at = None;
        self.connecting_error = None;
        let _ = self.to_net.send(UiToNet::Disconnect);
        self.is_connected = false;
        if !self.bg_sessions.is_empty() {
            let next_idx = active_pos.min(self.bg_sessions.len() - 1);
            self.switch_to_bg_session(next_idx, ctx);
        } else {
            self.screen = AppScreen::CharacterSelect;
        }
    }

    fn quit_all_sessions(&mut self, ctx: &egui::Context) {
        let _ = self.to_net.send(UiToNet::Disconnect);
        for bg in &self.bg_sessions {
            let _ = bg.to_net.send(UiToNet::Disconnect);
        }
        self.close_confirm_open = false;
        self.allow_quit = true;
        self.hidden_in_tray = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn log_chat_line(&self, text: &str) {
        if !self.logging_enabled {
            return;
        }
        if let Some(dir) = default_logs_dir() {
            let wire = self.active_character_wire_name();
            if self.shared_logs_mode {
                let _ = append_shared_chat_log(&dir, &wire, text);
            } else {
                let _ = append_chat_log(&dir, &wire, text);
            }
        }
    }

    fn log_whisper_line(&self, peer: &str, text: &str) {
        if !self.logging_enabled || !self.log_whispers {
            return;
        }
        if let Some(dir) = default_logs_dir() {
            let wire = self.active_character_wire_name();
            if self.shared_logs_mode {
                let _ = append_shared_whisper_log(&dir, &wire, peer, text);
            } else {
                let _ = append_whisper_log(&dir, &wire, peer, text);
            }
        }
    }

    fn load_whisper_history_for(&self, char_wire_or_disp: &str, peer: &str) -> Vec<String> {
        let Some(dir) = default_logs_root() else {
            return Vec::new();
        };
        if self.shared_logs_mode {
            load_shared_whisper_log(&dir, peer, 200)
        } else {
            let mut lines = load_whisper_log(&dir, char_wire_or_disp, peer, 200);
            if lines.is_empty() {
                let disp = char_wire_or_disp.replace('|', " ");
                if disp != char_wire_or_disp {
                    lines = load_whisper_log(&dir, &disp, peer, 200);
                }
            }
            lines
        }
    }

    fn launch_new_window() {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    fn run_windows_import(&mut self) {
        let src = std::path::PathBuf::from(self.import_path_input.trim());
        let existing: Vec<String> = self.pounce.entries().map(|e| e.name.clone()).collect();
        let summary = import_windows_furcadia_dir(&src, &existing, &self.active_character_name());
        if summary.assets_copied > 0 {
            self.assets = AssetLocator::discover();
            self.fonts_configured = false;
            self.palette = self.assets.load_default_palette().unwrap_or_default();
            self.pending_stock_patch_downloads.clear();
            self.reload_base_patch_caches();
            self.species_caches.clear();
            self.char_portraits.clear();
            self.invalidate_looked_card_textures();
        }
        for name in &summary.pounce_added {
            let wire = wire_player_name(name);
            self.pounce.add_watch(name.clone());
            if self.is_connected {
                self.send_raw_cmd(&format!("pounce adduser {wire}"));
            }
        }
        if !summary.pounce_added.is_empty() {
            self.save_config();
        }
        let mut msg = format!(
            "Imported {} asset(s), {} Pounce user(s), {} character INI(s), {} log file(s).",
            summary.assets_copied,
            summary.pounce_added.len(),
            summary.characters_found.len(),
            summary.logs_copied
        );
        if !summary.warnings.is_empty() {
            msg.push_str("\nWarnings: ");
            msg.push_str(&summary.warnings.join("; "));
        }
        self.import_report = Some(msg.clone());
        self.push_system_line("success", &msg);
    }

    /// Drops every in-memory avatar/portrait texture and archive so they are rebuilt
    /// (from disk or by re-downloading) the next time they are drawn.
    fn reset_avatar_memory_caches(&mut self) {
        self.species_caches.clear();
        self.pending_species_downloads.clear();
        self.char_portraits.clear();
        self.custom_portrait_rgba.clear();
        self.custom_portrait_textures.clear();
        self.remappable_portraits.clear();
        self.pending_portrait_downloads.clear();
        self.chat_img_textures.clear();
        self.pending_chat_img_downloads.clear();
        self.invalidate_looked_card_textures();
        // Re-request portraits for everyone we already know about.
        let pids: Vec<u32> = self.player_portrait_ids.values().copied().collect();
        for pid in pids {
            if self.pending_portrait_downloads.insert(pid) {
                let _ = self.to_net.send(UiToNet::DownloadPortrait { portrait_id: pid });
            }
        }
    }

    fn refresh_cache_usage(&mut self) {
        let species = AssetLocator::species_cache_dir()
            .map(|d| AssetLocator::dir_usage(&d))
            .unwrap_or((0, 0));
        let dreams = AssetLocator::dreams_cache_dir()
            .map(|d| AssetLocator::dir_usage(&d))
            .unwrap_or((0, 0));
        let patches = AssetLocator::stock_patches_cache_dir()
            .map(|d| AssetLocator::dir_usage(&d))
            .unwrap_or((0, 0));
        self.cache_usage = Some(CacheUsageStats {
            species,
            dreams,
            patches,
        });
    }

    fn retrigger_loaded_avatar_downloads(&mut self) {
        if self.world.avatars.is_empty() {
            return;
        }
        let mut file_ids = std::collections::BTreeSet::new();
        file_ids.insert(1usize);
        for av in self.world.avatars.values() {
            if let Some((eff_sp, _)) = self.world.resolved_avatar_species_gender(av) {
                file_ids.insert(self.resolve_species_spec(eff_sp as usize).0);
            }
            file_ids.insert(self.resolve_species_spec(av.appearance.species_id as usize).0);
        }
        for file_id in file_ids {
            self.ensure_species_cache(file_id);
        }
    }

    fn clear_avatar_download_cache(&mut self) {
        let result = AssetLocator::species_cache_dir()
            .ok_or_else(|| "could not resolve cache directory".to_owned())
            .and_then(|d| AssetLocator::clear_dir(&d).map_err(|e| e.to_string()));
        self.reset_avatar_memory_caches();
        self.retrigger_loaded_avatar_downloads();
        let msg = match result {
            Ok((files, bytes)) => format!(
                "Cleared {files} cached avatar/portrait file(s) ({}). They will re-download as needed.",
                format_bytes(bytes)
            ),
            Err(e) => format!("Could not clear avatar cache: {e}"),
        };
        self.cache_report = Some(msg.clone());
        self.push_system_line("system", &msg);
        self.refresh_cache_usage();
    }

    fn reload_or_clear_dream_after_cache_flush(&mut self) -> bool {
        if self.is_connected {
            if let Some(name) = self
                .last_dream_id
                .clone()
                .or_else(|| self.world.map_name.clone())
            {
                let modern = self.world.is_modern;
                let url = self.last_dream_explicit_url.clone();
                self.enter_dream(&name, None, url.as_deref(), false, Some(modern));
                return true;
            }
        }
        self.clear_dream_patches();
        false
    }

    fn clear_dream_download_cache(&mut self) {
        self.pending_stock_patch_downloads.clear();
        let result = AssetLocator::dreams_cache_dir()
            .ok_or_else(|| "could not resolve cache directory".to_owned())
            .and_then(|d| AssetLocator::clear_dir(&d).map_err(|e| e.to_string()));
        let mut msg = match result {
            Ok((files, bytes)) => format!("Cleared {files} cached dream file(s) ({}).", format_bytes(bytes)),
            Err(e) => format!("Could not clear dream cache: {e}"),
        };
        // The current dream's files were just deleted; fetch it again so patches keep working.
        if self.reload_or_clear_dream_after_cache_flush() {
            msg.push_str(" Re-downloading the current dream.");
        }
        self.cache_report = Some(msg.clone());
        self.push_system_line("system", &msg);
        self.refresh_cache_usage();
    }

    fn clear_all_download_caches(&mut self) {
        let species_res = AssetLocator::species_cache_dir()
            .ok_or_else(|| "could not resolve species cache directory".to_owned())
            .and_then(|d| AssetLocator::clear_dir(&d).map_err(|e| e.to_string()));
        let dreams_res = AssetLocator::dreams_cache_dir()
            .ok_or_else(|| "could not resolve dream cache directory".to_owned())
            .and_then(|d| AssetLocator::clear_dir(&d).map_err(|e| e.to_string()));
        let patches_res = AssetLocator::stock_patches_cache_dir()
            .ok_or_else(|| "could not resolve stock patch cache directory".to_owned())
            .and_then(|d| AssetLocator::clear_dir(&d).map_err(|e| e.to_string()));

        self.pending_stock_patch_downloads.clear();
        self.reload_base_patch_caches();
        let redownloading_dream = self.reload_or_clear_dream_after_cache_flush();
        if !redownloading_dream && (self.is_connected || self.world.map.is_some()) {
            self.ensure_stock_patches_for_current_map();
        }
        self.reset_avatar_memory_caches();
        self.retrigger_loaded_avatar_downloads();

        let msg = match (species_res, dreams_res, patches_res) {
            (Ok((sf, sb)), Ok((df, db)), Ok((pf, pb))) => {
                let mut s = format!(
                    "Cleared {} cached file(s) ({} avatars/portraits/images, {} dreams, {} stock patches; {}).",
                    sf + df + pf,
                    sf,
                    df,
                    pf,
                    format_bytes(sb + db + pb)
                );
                if redownloading_dream {
                    s.push_str(" Re-downloading current dream, stock patches, and active avatars.");
                } else {
                    s.push_str(" Cached files will re-download as needed.");
                }
                s
            }
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                format!("Could not clear all caches: {e}")
            }
        };
        self.cache_report = Some(msg.clone());
        self.push_system_line("system", &msg);
        self.refresh_cache_usage();
    }

    fn browse_import_directory(&mut self) {
        #[cfg(target_os = "windows")]
        let out = furcadia_client::sys::command("powershell")
            .args([
                "-NoProfile",
                "-STA",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; \
                 $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
                 $d.Description = 'Select Furcadia Folder'; \
                 if ($d.ShowDialog() -eq 'OK') { Write-Output $d.SelectedPath }",
            ])
            .output();
        #[cfg(target_os = "macos")]
        let out = std::process::Command::new("osascript")
            .args([
                "-e",
                "POSIX path of (choose folder with prompt \"Select Furcadia Folder\")",
            ])
            .output();
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let out = furcadia_client::sys::command("zenity")
            .args(["--file-selection", "--directory", "--title=Select Furcadia Windows Folder"])
            .output()
            .or_else(|_| {
                furcadia_client::sys::command("kdialog")
                    .args(["--getexistingdirectory", "."])
                    .output()
            });
        if let Ok(output) = out {
            if output.status.success() {
                let picked = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !picked.is_empty() {
                    self.import_path_input = picked;
                }
            }
        }
    }

    fn open_log_viewer(&mut self) {
        self.log_viewer_open = true;
        if self.log_viewer_char.is_empty() {
            self.log_viewer_char = if self.shared_logs_mode {
                SHARED_LOG_GROUP.to_owned()
            } else {
                self.active_character_wire_name()
            };
        }
        self.refresh_log_viewer_files();
    }

    fn refresh_log_viewer_files(&mut self) {
        self.log_viewer_files.clear();
        if let Some(dir) = default_logs_root() {
            let chars = list_log_characters(&dir);
            if !chars.is_empty() && !chars.iter().any(|c| c == &self.log_viewer_char) {
                let wire_folder = sanitized_log_character_name(&self.active_character_wire_name());
                let display_folder = sanitized_log_character_name(&self.active_character_name());
                self.log_viewer_char = chars
                    .iter()
                    .find(|c| **c == wire_folder || **c == display_folder)
                    .cloned()
                    .unwrap_or_else(|| chars[0].clone());
            }
            if !self.log_viewer_char.is_empty() {
                self.log_viewer_files = list_log_files(&dir, &self.log_viewer_char);
            }
        }
        if let Some(first) = self.log_viewer_files.first().cloned() {
            self.load_log_file_into_viewer(first);
        } else {
            self.log_viewer_selected = None;
            self.log_viewer_content.clear();
        }
    }

    fn load_log_file_into_viewer(&mut self, path: std::path::PathBuf) {
        let is_whisper = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("whispers"));
        if self.log_viewer_char == SHARED_LOG_GROUP && is_whisper {
            if let (Some(root), Some(stem)) = (
                default_logs_root(),
                path.file_stem().and_then(|s| s.to_str()),
            ) {
                let merged = load_shared_whisper_log(&root, stem, usize::MAX);
                if !merged.is_empty() {
                    self.log_viewer_content = merged.join("\n");
                    self.log_viewer_selected = Some(path);
                    return;
                }
            }
        }
        self.log_viewer_content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| format!("Failed to read {}: {e}", path.display()));
        self.log_viewer_selected = Some(path);
    }

    fn suspend_active_session_to_bg(&mut self, ctx: &egui::Context) {
        self.save_active_chat_draft();
        let (new_to_net, net_rx) = mpsc::channel::<UiToNet>();
        let (net_tx, new_from_net) = mpsc::channel::<NetToUi>();
        let net_ctx = ctx.clone();
        thread::spawn(move || network_worker(net_rx, net_tx, net_ctx));

        let old_to_net = std::mem::replace(&mut self.to_net, new_to_net);
        let old_from_net = std::mem::replace(&mut self.from_net, new_from_net);
        let old_world = std::mem::take(&mut self.world);
        let old_chat = std::mem::take(&mut self.chat_log);
        let old_whispers = std::mem::take(&mut self.whispers);
        let old_tab = self.active_chat_tab.take();
        let old_main_chat_draft = std::mem::take(&mut self.main_chat_draft);
        let old_pending_whisper_echoes = std::mem::take(&mut self.pending_whisper_echoes);
        self.chat_input.clear();
        let old_dream_dir = self.current_dream_dir.take();
        let old_status = std::mem::replace(&mut self.status_text, "Disconnected".to_owned());
        let old_connected = std::mem::replace(&mut self.is_connected, false);
        let old_cam = std::mem::replace(&mut self.cam_offset, Vec2::ZERO);
        let old_cookies_collected = self.cookies_collected.take();
        let old_cookies_total = self.cookies_total.take();
        let old_looked_player = self.looked_player.take();
        let old_main_looked_player = self.main_looked_player.take();
        self.pending_silent_tab_glooks.clear();
        let char_name = old_world
            .self_name
            .clone()
            .or_else(|| self.selected_character().map(|c| c.name.clone()))
            .unwrap_or_else(|| "Character".to_owned())
            .replace('|', " ");

        let insert_at = self.active_char_tab_pos.min(self.bg_sessions.len());
        self.bg_sessions.insert(
            insert_at,
            BackgroundSession {
                char_idx: self.selected_char_idx,
                char_name,
                live_desc_input: self.live_desc_input.clone(),
                is_connected: old_connected,
                status_text: old_status,
                world: old_world,
                chat_log: old_chat,
                whispers: old_whispers,
                active_chat_tab: old_tab,
                main_chat_draft: old_main_chat_draft,
                pending_whisper_echoes: old_pending_whisper_echoes,
                current_dream_dir: old_dream_dir,
                cam_offset: old_cam,
                unread_count: 0,
                cookies_collected: old_cookies_collected,
                cookies_total: old_cookies_total,
                looked_player: old_looked_player,
                main_looked_player: old_main_looked_player,
                to_net: old_to_net,
                from_net: old_from_net,
            },
        );
    }

    fn swap_char_tabs(&mut self, a: usize, b: usize) {
        let total = self.bg_sessions.len() + 1;
        if a == b || a >= total || b >= total {
            return;
        }
        let pos = self.active_char_tab_pos.min(self.bg_sessions.len());
        if a == pos {
            if a < b {
                for step in a..b {
                    let bg_idx = step;
                    if bg_idx < self.bg_sessions.len() && step + 1 != pos {
                        // Moving active right past bg[step] keeps relative order of bg_sessions.
                        let _ = bg_idx;
                    }
                }
            }
            // Move active from `pos` to `b`: remove nothing, shift bg elements between `pos` and `b`
            // Actually, if `pos == 0, bg=[B, C]` and `a=0, b=1`: `B` moves to 0, `active` moves to 1 -> `bg=[B, C]`, `pos=1`!
            // If `pos == 0, bg=[B, C]` and `a=0, b=2`: `B`->0, `C`->1, `active`->2 -> `bg=[B, C]`, `pos=2`!
            // Only `active_char_tab_pos = b` is needed when moving `active`!
            // Wait: if we SWAP `a` and `b` (not slide): `[A, B, C]` swap(0, 2) -> `[C, B, A]`, which means `C` moves to 0 and `A` moves to 2!
            let bg_b = if b < pos { b } else { b - 1 };
            let target = self.bg_sessions.remove(bg_b);
            let insert_at = if b < pos { pos - 1 } else { pos };
            self.bg_sessions.insert(insert_at.min(self.bg_sessions.len()), target);
            self.active_char_tab_pos = b;
        } else if b == pos {
            self.swap_char_tabs(b, a);
        } else {
            let bg_a = if a < pos { a } else { a - 1 };
            let bg_b = if b < pos { b } else { b - 1 };
            self.bg_sessions.swap(bg_a, bg_b);
        }
    }

    fn switch_to_bg_session(&mut self, idx: usize, ctx: &egui::Context) {
        if idx >= self.bg_sessions.len() {
            return;
        }
        let old_pos = self.active_char_tab_pos.min(self.bg_sessions.len());
        let target_vis_pos = if idx < old_pos { idx } else { idx + 1 };
        let target = self.bg_sessions.remove(idx);
        if self.is_connected {
            let insert_idx = if idx < old_pos {
                old_pos.saturating_sub(1)
            } else {
                old_pos
            };
            self.active_char_tab_pos = insert_idx;
            self.suspend_active_session_to_bg(ctx);
            self.active_char_tab_pos = target_vis_pos;
        } else {
            self.active_char_tab_pos = idx.min(self.bg_sessions.len());
        }
        self.selected_char_idx = target.char_idx;
        self.live_desc_input = target.live_desc_input;
        self.is_connected = target.is_connected;
        self.status_text = target.status_text;
        self.world = target.world;
        self.refresh_player_badge_colors();
        self.chat_log = target.chat_log;
        self.whispers = target.whispers;
        self.active_chat_tab = target.active_chat_tab;
        self.main_chat_draft = target.main_chat_draft;
        self.pending_whisper_echoes = target.pending_whisper_echoes;
        self.load_active_chat_draft();
        self.spell_seq = self.spell_seq.wrapping_add(1);
        self.spell_dirty_at = None;
        self.spell_misspelled.clear();
        self.cookies_collected = target.cookies_collected;
        self.cookies_total = target.cookies_total;
        if self.current_dream_dir != target.current_dream_dir {
            self.clear_dream_patches();
            if let Some(dir) = target.current_dream_dir {
                self.load_dream_dir(&dir);
            }
        }
        self.ensure_stock_patches_for_current_map();
        self.cam_offset = target.cam_offset;
        self.to_net = target.to_net;
        self.from_net = target.from_net;
        self.looked_player = target.looked_player;
        self.main_looked_player = target.main_looked_player;
        self.pending_silent_tab_glooks.clear();
        self.recent_portrait_target = None;
        self.context_menu_avatar = None;
        if self.active_chat_tab.is_some() && self.looked_player.is_none() {
            self.sync_portrait_for_active_chat_tab();
        }
        self.screen = AppScreen::InGame;
    }

    fn poll_bg_sessions(&mut self) {
        enum BgNotifyKind {
            Whisper,
            Mention,
            MainChatFlashOnly,
        }
        struct BgPortraitDownload {
            portrait_id: u32,
            rgba: Option<(usize, usize, Vec<u8>)>,
            remappable: bool,
        }
        let mut events_for_notify: Vec<(String, String, BgNotifyKind)> = Vec::new();
        let mut portrait_downloads: Vec<BgPortraitDownload> = Vec::new();
        let mut portrait_directives: Vec<String> = Vec::new();
        let mut stock_patch_downloads: Vec<(StockPatchSlot, Option<Box<FoxArchive>>)> = Vec::new();
        let nicknames_snapshot = self.mention_nicknames.clone();
        let notify_mention = self.notify_mention;
        let flash_on_main_chat = self.flash_taskbar_on_main_chat;
        for sess in &mut self.bg_sessions {
            while let Ok(ev) = sess.from_net.try_recv() {
                match ev {
                    NetToUi::Connected { connected, .. } => {
                        sess.is_connected = connected;
                        sess.status_text = if connected {
                            "Connected".to_owned()
                        } else {
                            "Disconnected".to_owned()
                        };
                    }
                    NetToUi::ConnectFailed { error, .. } => {
                        sess.is_connected = false;
                        sess.status_text = format!("Disconnected: {error}");
                    }
                    NetToUi::Message(ServerMessage::Text(t)) => {
                        let line = parse_chat_line(&t);
                        let plain = line.to_plain_text();
                        let (collected, total) = parse_cookie_counts(&plain);
                        if collected.is_some() {
                            sess.cookies_collected = collected;
                        }
                        if total.is_some() {
                            sess.cookies_total = total;
                        }
                        let peer = if let Some(peer) = extract_whisper_peer(&plain) {
                            Some(peer)
                        } else {
                            match_pending_whisper_echo(
                                &plain,
                                &sess.char_name,
                                &mut sess.pending_whisper_echoes,
                            )
                        };
                        if let Some(peer) = peer {
                            sess.unread_count += 1;
                            let formatted =
                                format_whisper_tab_line(&sess.char_name, &peer, &plain, &line);
                            if let Some(w) = sess
                                .whispers
                                .iter_mut()
                                .find(|w| w.peer.eq_ignore_ascii_case(&peer))
                            {
                                w.unread += 1;
                                push_bounded_chat_line(&mut w.lines, formatted);
                            } else {
                                let mut w = WhisperSession::new(peer.clone());
                                if let Some(dir) = default_logs_root() {
                                    w.history_lines = if self.shared_logs_mode {
                                        load_shared_whisper_log(&dir, &peer, 200)
                                    } else {
                                        load_whisper_log(&dir, &sess.char_name, &peer, 200)
                                    };
                                }
                                w.unread = 1;
                                push_bounded_chat_line(&mut w.lines, formatted);
                                sess.whispers.push(w);
                            }
                            if self.logging_enabled && self.log_whispers {
                                if let Some(dir) = default_logs_dir() {
                                    if self.shared_logs_mode {
                                        let _ = append_shared_whisper_log(&dir, &sess.char_name, &peer, &plain);
                                    } else {
                                        let _ = append_whisper_log(&dir, &sess.char_name, &peer, &plain);
                                    }
                                }
                            }
                            if !plain.starts_with("[ You whisper ") && !plain.starts_with("You whisper ") {
                                events_for_notify.push((
                                    format!("[{}] Whisper from {peer}", sess.char_name),
                                    plain.clone(),
                                    BgNotifyKind::Whisper,
                                ));
                            }
                        } else {
                            if notify_mention {
                                if let Some(matched) =
                                    find_chat_mention(&plain, &sess.char_name, &nicknames_snapshot)
                                {
                                    sess.unread_count += 1;
                                    events_for_notify.push((
                                        format!("[{}] Mentioned ({matched})", sess.char_name),
                                        plain.clone(),
                                        BgNotifyKind::Mention,
                                    ));
                                } else if flash_on_main_chat
                                    && !is_self_or_system_chat_line(&plain, &sess.char_name)
                                {
                                    events_for_notify.push((
                                        String::new(),
                                        String::new(),
                                        BgNotifyKind::MainChatFlashOnly,
                                    ));
                                }
                            } else if flash_on_main_chat
                                && !is_self_or_system_chat_line(&plain, &sess.char_name)
                            {
                                events_for_notify.push((
                                    String::new(),
                                    String::new(),
                                    BgNotifyKind::MainChatFlashOnly,
                                ));
                            }
                            push_bounded_chat_line(&mut sess.chat_log, line);
                        }
                    }
                    NetToUi::Message(ServerMessage::WorldPacket { opcode, data }) => {
                        sess.world.apply_world_packet(opcode, &data);
                    }
                    NetToUi::Message(ServerMessage::RegionDefaults(data)) => {
                        sess.world.apply_region_defaults(&data);
                    }
                    NetToUi::Message(ServerMessage::CharacterIdentity { uid, name }) => {
                        sess.world.self_uid = uid.parse().ok();
                        sess.world.self_name = Some(name.clone());
                        sess.char_name = name.replace('|', " ");
                        let self_wire = wire_player_name(&name);
                        if !self_wire.is_empty() {
                            let _ = sess
                                .to_net
                                .send(UiToNet::Send(ClientCommand::Glook(self_wire)));
                        }
                    }
                    NetToUi::Message(ServerMessage::Extended {
                        code: Some('&'),
                        payload,
                    }) => {
                        portrait_directives.push(payload);
                    }
                    NetToUi::Message(ServerMessage::ExtendedBytes { code: b'&', data }) => {
                        portrait_directives.push(furcadia_client::encoding::decode_win1252(&data));
                    }
                    NetToUi::PortraitDownloaded {
                        portrait_id,
                        rgba,
                        remappable,
                    } => {
                        portrait_downloads.push(BgPortraitDownload {
                            portrait_id,
                            rgba,
                            remappable,
                        });
                    }
                    NetToUi::StockPatchDownloaded { slot, archive } => {
                        stock_patch_downloads.push((slot, archive));
                    }
                    _ => {}
                }
            }
        }
        for (slot, archive) in stock_patch_downloads {
            self.apply_stock_patch_downloaded(slot, archive);
        }
        for dl in portrait_downloads {
            self.apply_portrait_downloaded(dl.portrait_id, dl.rgba, dl.remappable);
        }
        for raw in portrait_directives {
            self.handle_portrait_directive(&raw);
        }
        for (title, body, kind) in events_for_notify {
            match kind {
                BgNotifyKind::Whisper => {
                    if self.notify_whisper {
                        self.play_sound("whisper.wav");
                        self.send_desktop_notification(&title, &body);
                    }
                    if self.flash_taskbar_on_message {
                        self.request_taskbar_flash();
                    }
                }
                BgNotifyKind::Mention => {
                    if self.notify_mention {
                        self.play_sound("talk.wav");
                        self.send_desktop_notification(&title, &body);
                    }
                    if self.flash_taskbar_on_message {
                        self.request_taskbar_flash();
                    }
                }
                BgNotifyKind::MainChatFlashOnly => {
                    if self.flash_taskbar_on_main_chat {
                        self.request_taskbar_flash();
                    }
                }
            }
        }
    }

    fn trigger_login(&mut self) {
        if self.email.trim().is_empty() || self.password.is_empty() {
            self.auth_error = Some("Please enter your email and password.".into());
            return;
        }
        self.save_config();
        self.auth_busy = true;
        self.auth_error = None;
        let _ = self.to_net.send(UiToNet::AuthenticateAccount {
            email: self.email.trim().to_owned(),
            password: self.password.clone(),
        });
    }

    fn refresh_character_list(&mut self) {
        if let Some(ch) = self.selected_character() {
            self.saved_character = ch.name.clone();
        }
        self.trigger_login();
    }

    fn trigger_play(&mut self) {
        let Some(ch) = self.selected_character().cloned() else {
            return;
        };
        self.saved_character = ch.name.clone();
        self.save_config();
        self.world = WorldState::default();
        self.clear_dream_patches();
        self.pending_vascodagama = false;
        self.login_ack_at = None;
        self.login_trace_remaining = 0;
        self.pending_dream_modern = None;
        self.pending_dream_name = None;
        self.pending_dream_received_bytes = 0;
        self.pending_dream_total_bytes = None;
        self.pending_dream_stage.clear();
        self.reconnecting = false;
        self.chat_pinned_to_bottom = true;
        self.chat_scroll_to_bottom = true;
        self.looked_player = None;
        self.main_looked_player = None;
        self.pending_silent_tab_glooks.clear();
        for w in &mut self.whispers {
            w.portrait_fetched = false;
        }
        self.recent_portrait_target = None;
        self.hovered_avatar_uid = None;
        self.context_menu_avatar = None;
        self.cam_offset = Vec2::ZERO;
        self.live_desc_input = unescape_html(&ch.desc);
        self.chat_log.clear();
        self.pending_whisper_echoes.clear();
        self.is_connected = false;
        self.connected_at = None;
        self.connect_seq = self.connect_seq.wrapping_add(1);
        self.connecting_error = None;
        self.connecting_stage = format!("Connecting as {}...", ch.name);
        self.push_system_line("emit", &format!("Connecting as {}...", ch.name));
        self.screen = AppScreen::Connecting;
        let login_desc = self
            .pending_desc
            .take()
            .or_else(|| {
                let d = self.live_desc_input.trim();
                (!d.is_empty()).then(|| d.to_owned())
            })
            .map(|d| d.replace(['\r', '\n'], " "));
        let _ = self.to_net.send(UiToNet::ConnectCharacter {
            seq: self.connect_seq,
            email: self.email.trim().to_owned(),
            character: ch.name,
            password: self.password.clone(),
            updated_desc: login_desc,
            colors: (!ch.colors.is_empty()).then_some(ch.colors),
        });
    }

    fn reconnect_active_character(&mut self) {
        if self.is_connected || self.reconnecting {
            return;
        }
        self.reconnecting = true;
        self.pending_whisper_echoes.clear();
        self.pending_silent_tab_glooks.clear();
        for w in &mut self.whispers {
            w.portrait_fetched = false;
        }
        self.pending_vascodagama = false;
        self.login_ack_at = None;
        self.pending_dream_name = None;
        self.pending_dream_received_bytes = 0;
        self.pending_dream_total_bytes = None;
        self.pending_dream_stage.clear();
        self.pending_dream_modern = None;
        self.world.avatars.clear();
        self.world.map_name = None;
        self.chat_pinned_to_bottom = true;
        self.chat_scroll_to_bottom = true;
        let character = self.active_character_wire_name();
        let login_desc = self
            .pending_desc
            .take()
            .or_else(|| {
                let d = self.live_desc_input.trim();
                (!d.is_empty()).then(|| d.to_owned())
            })
            .map(|d| d.replace(['\r', '\n'], " "));
        let colors = self
            .selected_character()
            .and_then(|ch| (!ch.colors.is_empty()).then(|| ch.colors.clone()));
        self.connect_seq = self.connect_seq.wrapping_add(1);
        self.connecting_error = None;
        self.connecting_stage = "Reconnecting...".to_owned();
        self.push_system_line("emit", &format!("Reconnecting as {}...", character.replace('|', " ")));
        let _ = self.to_net.send(UiToNet::ConnectCharacter {
            seq: self.connect_seq,
            email: self.email.trim().to_owned(),
            character,
            password: self.password.clone(),
            updated_desc: login_desc,
            colors,
        });
    }
    
    fn push_system_line(&mut self, color: &str, text: &str) {
        push_bounded_chat_line(
            &mut self.chat_log,
            parse_chat_line(&format!("<font color='{color}'>{text}</font>")),
        );
        if self.chat_pinned_to_bottom {
            self.chat_scroll_to_bottom = true;
        }
    }

    fn sanitize_dream_id(name: &str) -> String {
        let base = name.trim().trim_end_matches(".map");
        base.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    }

    fn clear_dream_patches(&mut self) {
        self.floors_cache.clear_dream_patch();
        self.floors2_cache.clear_dream_patch();
        self.floore_cache.clear_dream_patch();
        self.items_cache.clear_dream_patch();
        self.iteme_cache.clear_dream_patch();
        self.portals_cache.clear_dream_patch();
        self.portalse_cache.clear_dream_patch();
        self.effects_cache.clear_dream_patch();
        self.effectse_cache.clear_dream_patch();
        self.lighting_cache.clear_dream_patch();
        self.lightinge_cache.clear_dream_patch();
        self.ambience_cache.clear_dream_patch();
        self.ambiencee_cache.clear_dream_patch();
        self.dream_item_splits.clear();
        self.junction_cache.clear_dream_patch();
        for cache in self.wall_caches.values_mut() {
            cache.clear_dream_patch();
        }
        self.species_caches.clear();
        self.char_portraits.clear();
        self.current_dream_dir = None;
        self.custom_dream_skin = None;
        self.stop_music();
    }

    fn bookmark_dream(&mut self, raw_url: &str) {
        let trimmed = raw_url.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("furc://") {
            return;
        }
        let normalized = if trimmed.starts_with("furc://") {
            trimmed.to_owned()
        } else {
            format!("furc://{}", trimmed.trim_end_matches(".map"))
        };
        if !self
            .dream_bookmarks
            .iter()
            .any(|d| d.eq_ignore_ascii_case(&normalized))
        {
            self.dream_bookmarks.push(normalized);
            self.save_config();
        }
    }

    fn find_file_case_insensitive(dir: &std::path::Path, target: &str) -> Option<std::path::PathBuf> {
        let exact = dir.join(target);
        if exact.is_file() {
            return Some(exact);
        }
        let entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
        for entry in &entries {
            let path = entry.path();
            if path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.eq_ignore_ascii_case(target))
            {
                return Some(path);
            }
        }
        // Check one subdirectory level in case an archive wrapped files inside a folder.
        for entry in entries {
            let sub = entry.path();
            if sub.is_dir() {
                if let Ok(sub_entries) = std::fs::read_dir(&sub) {
                    for sub_entry in sub_entries.flatten() {
                        let path = sub_entry.path();
                        if path.is_file()
                            && path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.eq_ignore_ascii_case(target))
                        {
                            return Some(path);
                        }
                    }
                }
            }
        }
        None
    }

    fn load_dream_dir(&mut self, dream_dir: &std::path::Path) {
        self.current_dream_dir = Some(dream_dir.to_path_buf());
        self.custom_dream_skin = Self::parse_dream_skin_ini(dream_dir);
        self.play_dream_music(dream_dir);
        let mut patch_warnings = Vec::new();
        fn collect_err(warnings: &mut Vec<String>, res: Result<(), String>) {
            if let Err(err) = res {
                warnings.push(err);
            }
        }

        fn find_patch_file(dir: &std::path::Path, stem: &str) -> Option<std::path::PathBuf> {
            FurcadiaApp::find_file_case_insensitive(dir, &format!("{stem}.fox"))
                .or_else(|| FurcadiaApp::find_file_case_insensitive(dir, &format!("{stem}.fsh")))
        }

        if let Some(p) =
            find_patch_file(dream_dir, "floors").or_else(|| find_patch_file(dream_dir, "floor"))
        {
            collect_err(&mut patch_warnings, self.floors_cache.set_dream_patch(&p));
        }
        for (stem, target) in [
            ("floors2", &mut self.floors2_cache),
            ("floore", &mut self.floore_cache),
            ("items", &mut self.items_cache),
            ("iteme", &mut self.iteme_cache),
            ("portals", &mut self.portals_cache),
            ("portalse", &mut self.portalse_cache),
            ("effects", &mut self.effects_cache),
            ("effectse", &mut self.effectse_cache),
            ("lighting", &mut self.lighting_cache),
            ("lightinge", &mut self.lightinge_cache),
            ("ambience", &mut self.ambience_cache),
            ("ambiencee", &mut self.ambiencee_cache),
            ("junction", &mut self.junction_cache),
        ] {
            if let Some(p) = find_patch_file(dream_dir, stem) {
                collect_err(&mut patch_warnings, target.set_dream_patch(&p));
            }
        }
        for (stem, min_id, max_id, offset) in [
            ("item", 1u16, 304u16, 0u16),
            ("item2", 305, 999, 305),
            ("item3", 1000, 1348, 1000),
            ("item4", 1349, 1748, 1349),
        ] {
            if let Some(p) = find_patch_file(dream_dir, stem) {
                let cache = FoxSpriteCache::from_dream_only(&p, stem);
                if !cache.is_loaded() {
                    patch_warnings.push(format!("{stem}: failed to parse patch archive"));
                }
                self.dream_item_splits.push((min_id, max_id, offset, cache));
            }
        }
        if let Some(wall0) =
            find_patch_file(dream_dir, "wall").or_else(|| find_patch_file(dream_dir, "wall_0"))
        {
            if let Some(c) = self.wall_caches.get_mut(&0) {
                collect_err(&mut patch_warnings, c.set_dream_patch(&wall0));
            }
        }
        for g in 1..=20 {
            let stem = format!("wall_{g}");
            if let Some(p) = find_patch_file(dream_dir, &stem) {
                if let Some(c) = self.wall_caches.get_mut(&g) {
                    collect_err(&mut patch_warnings, c.set_dream_patch(&p));
                } else {
                    let cache = FoxSpriteCache::from_dream_only(&p, &stem);
                    if !cache.is_loaded() {
                        patch_warnings.push(format!("{stem}: failed to parse patch archive"));
                    }
                    self.wall_caches.insert(g, cache);
                }
            }
        }
        for warn in patch_warnings {
            self.push_system_line("warning", &format!("Dream patch decode warning ({warn})"));
        }
    }

    fn find_map_in_dir(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        if let Some(default_map) = Self::find_file_case_insensitive(dir, "default.map") {
            return Some(default_map);
        }
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("map"))
            {
                candidates.push(path);
            } else if path.is_dir() {
                if let Ok(sub) = std::fs::read_dir(&path) {
                    for se in sub.flatten() {
                        let sp = se.path();
                        if sp.is_file()
                            && sp
                                .extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| e.eq_ignore_ascii_case("map"))
                        {
                            candidates.push(sp);
                        }
                    }
                }
            }
        }
        candidates.sort();
        candidates.into_iter().next()
    }

    fn try_load_cached_dream(
        &mut self,
        name: &str,
        expected_checksum: Option<&str>,
    ) -> Option<(FurcadiaMap, std::path::PathBuf)> {
        let dreams_dir = AssetLocator::dreams_cache_dir()?;
        let dir = dreams_dir.join(Self::sanitize_dream_id(name));
        if let Some(expected) = expected_checksum {
            let cached_hash = std::fs::read_to_string(dir.join(".hash")).ok()?;
            if cached_hash.trim() != expected.trim() {
                return None;
            }
        } else {
            // Without a server checksum, never trust an old dream package forever:
            // map edits can otherwise leave stale walls and floors in the live view.
            let modified = std::fs::metadata(&dir).ok()?.modified().ok()?;
            let age = std::time::SystemTime::now().duration_since(modified).ok()?;
            if age > Duration::from_secs(6 * 60 * 60) {
                return None;
            }
        }
        let map_path = Self::find_map_in_dir(&dir)?;
        let map = FurcadiaMap::load(&map_path).ok()?;
        Some((map, dir))
    }

    fn send_pending_vascodagama(&mut self) {
        self.login_ack_at = None;
        if std::mem::take(&mut self.pending_vascodagama) && self.is_connected {
            let _ = self
                .to_net
                .send(UiToNet::Send(ClientCommand::Raw("vascodagama".to_owned())));
        }
    }

    fn handle_extended_dream(&mut self, payload: &str) {
        let tokens: Vec<&str> = payload.split_whitespace().collect();
        let is_modern = tokens.contains(&"modern");
        let explicit_url = tokens
            .iter()
            .copied()
            .find(|t| t.starts_with("http://") || t.starts_with("https://"));
        self.pending_vascodagama = true;
        if let Some(url) = explicit_url {
            let dream_id = tokens
                .first()
                .copied()
                .filter(|t| {
                    !t.starts_with("http://") && !t.starts_with("https://") && *t != "modern"
                })
                .map(str::to_owned)
                .or_else(|| self.pending_dream_name.clone())
                .unwrap_or_else(|| "custom_dream".to_owned());
            let checksum = tokens.iter().copied().find(|t| {
                !t.starts_with("http://")
                    && !t.starts_with("https://")
                    && *t != dream_id
                    && *t != "modern"
            });
            self.enter_dream(&dream_id, checksum, Some(url), false, Some(is_modern));
        } else if let Some(&dream_id) = tokens.first().filter(|t| **t != "modern") {
            let checksum = tokens.get(1).copied().filter(|t| *t != "modern");
            self.enter_dream(dream_id, checksum, None, false, Some(is_modern));
        } else {
            self.world.is_modern = is_modern;
            self.send_pending_vascodagama();
        }
    }

    fn handle_use_map(&mut self, name: &str) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            // The server sends `]J` with an empty payload right after `]q <dream_id>` when
            // entering a player dream (indicating no legacy main-map override). Do not wipe
            // the dream that `]q` just loaded or started downloading.
            return;
        }
        let lower = trimmed.trim_end_matches(".map").to_ascii_lowercase();
        let normalized = normalize_main_map_id(&lower).unwrap_or(trimmed);
        let already_pending = self.pending_dream_name.as_deref().is_some_and(|p| {
            p.trim()
                .trim_end_matches(".map")
                .eq_ignore_ascii_case(normalized.trim_end_matches(".map"))
        });
        if already_pending {
            return;
        }
        self.enter_dream(normalized, None, None, false, None);
    }

    /// Returns (and caches) a species portrait (`purpose = 4`) colorized for `app`.
    fn portrait_for_appearance(
        &mut self,
        ctx: &egui::Context,
        app: &AvatarAppearance,
        key: String,
    ) -> Option<TextureHandle> {
        if let Some(t) = self.char_portraits.get(&key) {
            return t.clone();
        }
        let sp = if app.species_id == 0 { 1 } else { app.species_id };
        let (file_id, obj_id) = self.resolve_species_spec(sp as usize);
        let ck = self.ensure_species_cache(file_id);
        let palette = &self.palette;
        let tex = self.species_caches.get_mut(&ck).and_then(|c| {
            c.get_or_load_portrait_tex(ctx, palette, app, obj_id, app.gender, 4)
        });
        self.char_portraits.insert(key, tex.clone());
        tex
    }

    /// Portrait for an account character from its `clogin.php` color code.
    fn character_portrait(
        &mut self,
        ctx: &egui::Context,
        id: &str,
        colors: &str,
    ) -> Option<TextureHandle> {
        if colors.is_empty() {
            return None;
        }
        let app = furcadia_client::parse_color_code(colors);
        self.portrait_for_appearance(
            ctx,
            &app,
            format!("char:{id}:{colors}:{}:{}:{:?}", app.species_id, app.gender, app.color_key),
        )
    }

    /// Prefer the character's saved custom port when one is known.
    fn account_character_portrait(
        &mut self,
        ctx: &egui::Context,
        id: &str,
        name: &str,
        colors: &str,
        last_port: Option<u32>,
    ) -> Option<TextureHandle> {
        let name_key = shortname_key(name);
        let app = furcadia_client::parse_color_code(colors);
        if let Some(pid) = resolve_custom_portrait_id(&self.player_portrait_ids, &name_key, last_port)
        {
            if let Some(tex) = self.custom_portrait_texture(ctx, pid, &app) {
                return Some(tex);
            }
        }
        self.character_portrait(ctx, id, colors)
    }

    /// Keep the account picker in sync with the authoritative appearance received in-world.
    fn sync_selected_character_colors(&mut self) {
        let live = self
            .world
            .self_uid
            .and_then(|uid| self.world.avatars.get(&uid))
            .map(|avatar| (avatar.name.clone(), avatar.colors.clone()));
        let Some((name, colors)) = live.filter(|(_, colors)| !colors.is_empty()) else {
            return;
        };
        let Some(info) = self.account_info.as_mut() else {
            return;
        };
        let live_key = shortname_key(&name);
        if let Some(character) = info
            .characters
            .iter_mut()
            .find(|character| shortname_key(&character.name) == live_key)
        {
            if character.colors != colors {
                character.colors = colors;
                self.char_portraits.clear();
            }
        }
    }

    /// Portrait for the logged-in character (live appearance, falling back to account colors).
    fn custom_portrait_texture(
        &mut self,
        ctx: &egui::Context,
        pid: u32,
        appearance: &AvatarAppearance,
    ) -> Option<TextureHandle> {
        let remappable = self.remappable_portraits.contains(&pid);
        let key = if remappable {
            format!("custom_portrait_{pid}:{:?}", appearance.color_key)
        } else {
            format!("custom_portrait_{pid}")
        };
        if let Some(tex) = self.custom_portrait_textures.get(&key) {
            return Some(tex.clone());
        }
        if let Some(Some((w, h, pixels))) = self.custom_portrait_rgba.get(&pid) {
            let pixels = if remappable {
                RemapLuts::from_appearance(appearance).apply_rgba(pixels)
            } else {
                pixels.clone()
            };
            let img = ColorImage::from_rgba_unmultiplied([*w, *h], &pixels);
            let tex = ctx.load_texture(
                key.clone(),
                img,
                TextureOptions::LINEAR,
            );
            if self.custom_portrait_textures.len() >= MAX_CUSTOM_PORTRAITS {
                self.custom_portrait_textures.clear();
            }
            self.custom_portrait_textures.insert(key, tex.clone());
            return Some(tex);
        }
        if !self.custom_portrait_rgba.contains_key(&pid)
            && self.pending_portrait_downloads.insert(pid)
        {
            let _ = self
                .to_net
                .send(UiToNet::DownloadPortrait { portrait_id: pid });
        }
        None
    }

    fn current_player_badge_textures(
        &mut self,
        ctx: &egui::Context,
    ) -> HashMap<String, TextureHandle> {
        let avatars: Vec<_> = self
            .world
            .avatars
            .values()
            .filter_map(|av| {
                let (species, gender) = self.world.resolved_avatar_species_gender(av)?;
                Some((
                    shortname_key(&av.name),
                    av.name.clone(),
                    species,
                    gender,
                    av.appearance.clone(),
                ))
            })
            .collect();
        let mut textures = HashMap::new();
        for (name, display_name, species, gender, appearance) in avatars {
            if name.is_empty() {
                continue;
            }
            // Chat tags are the species/gender FOX marker (purpose 35).
            // Custom portraits belong in profile cards, not in the tag slot.
            let (file_id, obj_id) = self.resolve_species_spec(species as usize);
            let cache_id = self.ensure_species_cache(file_id);
            if let Some(cache) = self.species_caches.get_mut(&cache_id) {
                if let Some(tex) = cache.get_or_load_portrait_tex(
                    ctx,
                    &self.palette,
                    &appearance,
                    obj_id,
                    gender,
                    35,
                ) {
                    textures.insert(shortname_key(&display_name), tex);
                }
            }
        }
        textures
    }

    fn refresh_player_badge_colors(&mut self) {
        self.player_badge_colors.clear();
        if let Some(self_name) = self.world.self_name.as_deref() {
            let key = shortname_key(self_name);
            if !key.is_empty() {
                self.player_badge_colors
                    .insert(key, name_badge_chat_color(true));
            }
        }
        for avatar in self.world.avatars.values() {
            let key = shortname_key(&avatar.name);
            if !key.is_empty() {
                self.player_badge_colors.insert(
                    key,
                    name_badge_chat_color(Some(avatar.uid) == self.world.self_uid),
                );
            }
        }
    }

    fn self_custom_portrait_id(&self) -> Option<u32> {
        let self_name = self.world.self_name.as_deref().unwrap_or("");
        let active_name = self.active_character_name();
        let target_key = if !self_name.is_empty() {
            shortname_key(self_name)
        } else {
            shortname_key(&active_name)
        };
        if target_key.is_empty() {
            return None;
        }
        let last_port = self.account_info.as_ref().and_then(|info| {
            info.characters
                .iter()
                .find(|c| shortname_key(&c.name) == target_key)
                .and_then(|c| c.last_port)
        });
        resolve_custom_portrait_id(&self.player_portrait_ids, &target_key, last_port)
    }

    fn self_portrait(&mut self, ctx: &egui::Context) -> Option<TextureHandle> {
        let live = self
            .world
            .self_uid
            .and_then(|uid| self.world.avatars.get(&uid))
            .map(|a| a.appearance.clone());
        if let Some(app) = live {
            if let Some(pid) = self.self_custom_portrait_id() {
                if let Some(tex) = self.custom_portrait_texture(ctx, pid, &app) {
                    return Some(tex);
                }
            }
            let key = format!("self:{}:{}:{:?}", app.species_id, app.gender, app.color_key);
            return self.portrait_for_appearance(ctx, &app, key);
        }
        let (id, colors) = self
            .selected_character()
            .map(|c| (c.id.clone(), c.colors.clone()))?;
        if let Some(pid) = self.self_custom_portrait_id() {
            let app = furcadia_client::parse_color_code(&colors);
            if let Some(tex) = self.custom_portrait_texture(ctx, pid, &app) {
                return Some(tex);
            }
        }
        self.character_portrait(ctx, &id, &colors)
    }

    fn resolve_species_spec(&self, sp_id: usize) -> (usize, u16) {
        resolve_species_spec_with_dynamic(sp_id, &self.dynamic_avatars)
    }

    /// Parses `]M%<server_code><8-byte entries>` (`DynamicAvatarDetails`) from `Furcadia.exe`.
    fn apply_dynamic_avatar_details(&mut self, data: &[u8]) {
        let Some((server_code, entries)) = parse_dynamic_avatar_details(data) else {
            return;
        };
        self.dynamic_avatar_server = server_code;
        let mut changed = false;
        for (species_id, dplayer_id, hop) in entries {
            let prev = self.dynamic_avatars.insert(species_id, (dplayer_id, hop));
            if prev != Some((dplayer_id, hop)) {
                changed = true;
                self.species_caches.remove(&(species_id as usize));
            }
        }
        if changed {
            self.char_portraits.clear();
            self.invalidate_looked_card_textures();
        }
    }

    fn apply_portrait_downloaded(
        &mut self,
        portrait_id: u32,
        rgba: Option<(usize, usize, Vec<u8>)>,
        remappable: bool,
    ) {
        self.pending_portrait_downloads.remove(&portrait_id);
        self.cache_usage = None;
        if remappable {
            self.remappable_portraits.insert(portrait_id);
        }
        if self.custom_portrait_rgba.len() >= MAX_CUSTOM_PORTRAITS {
            self.custom_portrait_rgba.clear();
            self.custom_portrait_textures.clear();
        }
        self.custom_portrait_rgba.insert(portrait_id, rgba);
    }

    /// Parses Furcadia's `]&<character-id> pp<portrait-file-id>` portrait directive.
    fn handle_portrait_directive(&mut self, raw: &str) {
        let (character_id, pid, name_parts) = parse_portrait_directive(raw);
        let Some(pid) = pid else {
            return;
        };
        let pending_target = self
            .pending_glook_target
            .as_ref()
            .filter(|(_, at)| at.elapsed() <= Duration::from_secs(3))
            .map(|(name, _)| name.clone());
        let recent_target = self
            .recent_portrait_target
            .as_ref()
            .filter(|(_, at)| at.elapsed() <= Duration::from_secs(3))
            .map(|(name, _)| name.clone());
        let account_match = character_id.and_then(|cid| {
            self.account_info.as_ref().and_then(|info| {
                info.characters
                    .iter()
                    .find(|c| c.id.parse::<u32>().ok() == Some(cid))
                    .map(|c| wire_player_name(&c.name))
            })
        });
        let target_name = name_parts
            .filter(|name| !name.is_empty())
            .or(account_match)
            .or_else(|| {
                character_id
                    .and_then(|uid| self.world.avatars.get(&uid))
                    .map(|avatar| wire_player_name(&avatar.name))
            })
            .or_else(|| {
                character_id
                    .filter(|uid| Some(*uid) == self.world.self_uid)
                    .and(self.world.self_name.as_deref())
                    .map(wire_player_name)
            })
            .or_else(|| {
                character_id.and_then(|uid| {
                    self.bg_sessions.iter().find_map(|sess| {
                        sess.world
                            .avatars
                            .get(&uid)
                            .map(|avatar| wire_player_name(&avatar.name))
                            .or_else(|| {
                                (sess.world.self_uid == Some(uid))
                                    .then_some(sess.world.self_name.as_deref())
                                    .flatten()
                                    .map(wire_player_name)
                            })
                    })
                })
            })
            .or(pending_target.clone())
            .or(recent_target.clone());
        if let Some(cid) = character_id {
            if let Some(info) = self.account_info.as_mut() {
                if let Some(ch) = info
                    .characters
                    .iter_mut()
                    .find(|c| c.id.parse::<u32>().ok() == Some(cid))
                {
                    ch.last_port = Some(pid);
                }
            }
        }
        if let Some(ref name) = target_name {
            let key = shortname_key(name);
            if !key.is_empty() {
                let prev = self.player_portrait_ids.insert(key.clone(), pid);
                let is_account_char = self
                    .account_info
                    .as_ref()
                    .is_some_and(|info| info.characters.iter().any(|c| shortname_key(&c.name) == key))
                    || self
                        .world
                        .self_name
                        .as_deref()
                        .is_some_and(|n| shortname_key(n) == key)
                    || shortname_key(&self.active_character_name()) == key;
                if is_account_char && prev != Some(pid) {
                    self.save_config();
                }
                if let Some(card) = self.looked_player.as_mut() {
                    if shortname_key(&card.short_name) == key {
                        card.portrait_id = Some(pid);
                    }
                }
                if let Some(card) = self.main_looked_player.as_mut() {
                    if shortname_key(&card.short_name) == key {
                        card.portrait_id = Some(pid);
                    }
                }
                if let Some(card) = self.known_player_cards.get_mut(&key) {
                    card.portrait_id = Some(pid);
                }
                for w in &mut self.whispers {
                    if let Some(card) = w.looked_card.as_mut() {
                        if shortname_key(&card.short_name) == key {
                            card.portrait_id = Some(pid);
                        }
                    }
                }
            }
        }
        if pending_target
            .as_deref()
            .is_some_and(|n| target_name.as_deref().is_some_and(|t| shortname_key(n) == shortname_key(t)))
        {
            self.pending_glook_target = None;
        }
        if recent_target
            .as_deref()
            .is_some_and(|n| target_name.as_deref().is_some_and(|t| shortname_key(n) == shortname_key(t)))
        {
            self.recent_portrait_target = None;
        }
        if !self.custom_portrait_rgba.contains_key(&pid)
            && self.pending_portrait_downloads.insert(pid)
        {
            let _ = self.to_net.send(UiToNet::DownloadPortrait { portrait_id: pid });
        }
    }

    fn invalidate_looked_card_textures(&mut self) {
        if let Some(card) = self.looked_player.as_mut() {
            card.resolved_for = None;
        }
        if let Some(card) = self.main_looked_player.as_mut() {
            card.resolved_for = None;
        }
        for card in self.known_player_cards.values_mut() {
            card.resolved_for = None;
        }
        for w in &mut self.whispers {
            if let Some(card) = w.looked_card.as_mut() {
                card.resolved_for = None;
            }
        }
        for sess in &mut self.bg_sessions {
            if let Some(card) = sess.looked_player.as_mut() {
                card.resolved_for = None;
            }
            if let Some(card) = sess.main_looked_player.as_mut() {
                card.resolved_for = None;
            }
            for w in &mut sess.whispers {
                if let Some(card) = w.looked_card.as_mut() {
                    card.resolved_for = None;
                }
            }
        }
    }

    fn request_silent_whisper_glook(&mut self, peer: &str) {
        if !self.is_connected {
            return;
        }
        let target = wire_player_name(peer);
        let key = shortname_key(&target);
        if key.is_empty() {
            return;
        }
        self.pending_silent_tab_glooks
            .retain(|e| e.sent_at.elapsed() <= Duration::from_secs(5));
        if self
            .pending_silent_tab_glooks
            .iter()
            .any(|e| e.target_key == key)
        {
            return;
        }
        if self.pending_silent_tab_glooks.len() >= 8 {
            self.pending_silent_tab_glooks.remove(0);
        }
        self.pending_silent_tab_glooks.push(SilentTabGlook {
            target_key: key,
            sent_at: Instant::now(),
            card_received: false,
            saw_you_see: false,
        });
        self.pending_glook_target = Some((target.clone(), Instant::now()));
        let _ = self.to_net.send(UiToNet::Send(ClientCommand::Glook(target)));
    }

    fn sync_portrait_for_active_chat_tab(&mut self) {
        let glook_peer = sync_tab_portrait_state(
            self.active_chat_tab.as_deref(),
            self.is_connected,
            &mut self.looked_player,
            &mut self.main_looked_player,
            &mut self.whispers,
            &self.known_player_cards,
            &self.player_portrait_ids,
            &self.world.avatars,
        );
        if let Some(peer) = glook_peer {
            self.request_silent_whisper_glook(&peer);
        }
        if let Some(pid) = self.looked_player.as_ref().and_then(|c| c.portrait_id) {
            if !self.custom_portrait_rgba.contains_key(&pid)
                && self.pending_portrait_downloads.insert(pid)
            {
                let _ = self.to_net.send(UiToNet::DownloadPortrait { portrait_id: pid });
            }
        }
    }

    fn ensure_species_cache(&mut self, file_id: usize) -> usize {
        if !self.species_caches.contains_key(&file_id) {
            let assets = &self.assets;
            let dream_dir = self.current_dream_dir.as_deref();
            let mut cache = if file_id >= DYNAMIC_AVATAR_CACHE_BASE {
                let dplayer_id = file_id - DYNAMIC_AVATAR_CACHE_BASE;
                let cached = AssetLocator::species_cache_dir()
                    .map(|d| d.join(format!("dplayer{dplayer_id}.fox")))
                    .filter(|p| p.is_file());
                if let Some(cached_p) = cached {
                    let mut tmp =
                        FoxSpriteCache::from_dream_only(&cached_p, &format!("dplayer_{dplayer_id}"));
                    tmp.default_archive = tmp.dream_archive.take();
                    tmp
                } else {
                    FoxSpriteCache::from_dream_only(
                        std::path::Path::new(""),
                        &format!("dplayer_{dplayer_id}"),
                    )
                }
            } else if file_id > 1000 {
                let sp_name = format!("species{file_id}.fox");
                let c = FoxSpriteCache::load(assets, &sp_name, &format!("species_{file_id}"));
                if c.is_loaded() {
                    c
                } else {
                    FoxSpriteCache::load_local_species(
                        assets,
                        file_id - 1000,
                        &format!("local_{}", file_id - 1000),
                    )
                }
            } else {
                let mut c = if let Some(cached_p) = assets.species_patch_path(file_id) {
                    let mut tmp =
                        FoxSpriteCache::from_dream_only(&cached_p, &format!("species_{file_id}"));
                    tmp.default_archive = tmp.dream_archive.take();
                    tmp
                } else {
                    FoxSpriteCache::load(
                        assets,
                        &format!("species{file_id}.fox"),
                        &format!("species_{file_id}"),
                    )
                };
                if !c.is_loaded() {
                    c = FoxSpriteCache::load(
                        assets,
                        &format!("species{file_id}.fox"),
                        &format!("species_{file_id}"),
                    );
                }
                c
            };
            if let Some(dir) = dream_dir {
                let patch_path = if file_id > 1000 {
                    Self::find_file_case_insensitive(dir, &format!("species{file_id}.fox"))
                        .or_else(|| {
                            Self::find_file_case_insensitive(
                                dir,
                                &format!("local{}.fox", file_id - 1000),
                            )
                        })
                } else {
                    Self::find_file_case_insensitive(dir, &format!("species{file_id}.fox"))
                };
                if let Some(p) = patch_path {
                    let _ = cache.set_dream_patch(&p);
                }
            }
            self.species_caches.insert(file_id, cache);
        }
        let is_loaded = self
            .species_caches
            .get(&file_id)
            .is_some_and(FoxSpriteCache::is_loaded);
        if file_id >= DYNAMIC_AVATAR_CACHE_BASE {
            if self.pending_species_downloads.insert(file_id) {
                let dplayer_id = file_id - DYNAMIC_AVATAR_CACHE_BASE;
                let cached = AssetLocator::species_cache_dir()
                    .map(|d| d.join(format!("dplayer{dplayer_id}.fox")))
                    .filter(|p| p.is_file());
                let _ = self.to_net.send(UiToNet::DownloadSpecies {
                    file_id,
                    server_code: self.dynamic_avatar_server,
                    if_newer_than: cached,
                });
            }
        } else if !is_loaded
            && ((1..1000).contains(&file_id) || (1001..=1004).contains(&file_id))
            && self.pending_species_downloads.insert(file_id)
        {
            let _ = self.to_net.send(UiToNet::DownloadSpecies {
                file_id,
                server_code: None,
                if_newer_than: None,
            });
        }
        if is_loaded {
            file_id
        } else {
            if !self.species_caches.contains_key(&1) {
                let mut c1 = if let Some(p1) = self.assets.species_patch_path(1) {
                    let mut tmp = FoxSpriteCache::from_dream_only(&p1, "species_1");
                    tmp.default_archive = tmp.dream_archive.take();
                    tmp
                } else {
                    FoxSpriteCache::load(&self.assets, "species1.fox", "species_1")
                };
                if !c1.is_loaded() && self.pending_species_downloads.insert(1) {
                    let _ = self.to_net.send(UiToNet::DownloadSpecies {
                        file_id: 1,
                        server_code: None,
                        if_newer_than: None,
                    });
                }
                if let Some(p) = self
                    .current_dream_dir
                    .as_deref()
                    .and_then(|d| Self::find_file_case_insensitive(d, "species1.fox"))
                {
                    let _ = c1.set_dream_patch(&p);
                }
                self.species_caches.insert(1, c1);
            }
            1
        }
    }

    fn has_stock_patch_loaded(&self, slot: StockPatchSlot) -> bool {
        match slot {
            StockPatchSlot::ModernFloors => self.floors_cache.default_archive.is_some(),
            StockPatchSlot::LegacyFloors => self.floors_cache.legacy_archive.is_some(),
            StockPatchSlot::LegacyFloors2 => self.floors2_cache.legacy_archive.is_some(),
            StockPatchSlot::ModernFloore => self.floore_cache.default_archive.is_some(),
            StockPatchSlot::LegacyFloore => self.floore_cache.legacy_archive.is_some(),
            StockPatchSlot::ModernItems => self.items_cache.default_archive.is_some(),
            StockPatchSlot::LegacyItem => self.items_cache.legacy_archive.is_some(),
            StockPatchSlot::LegacyItem2 => self
                .legacy_item_splits
                .iter()
                .any(|(min_id, _, _, c)| *min_id == 305 && c.legacy_archive.is_some()),
            StockPatchSlot::LegacyItem3 => self
                .legacy_item_splits
                .iter()
                .any(|(min_id, _, _, c)| *min_id == 1000 && c.legacy_archive.is_some()),
            StockPatchSlot::LegacyItem4 => self
                .legacy_item_splits
                .iter()
                .any(|(min_id, _, _, c)| *min_id == 1349 && c.legacy_archive.is_some()),
            StockPatchSlot::ModernIteme => self.iteme_cache.default_archive.is_some(),
            StockPatchSlot::LegacyIteme => self.iteme_cache.legacy_archive.is_some(),
            StockPatchSlot::ModernPortals => self.portals_cache.default_archive.is_some(),
            StockPatchSlot::ModernPortalse => self.portalse_cache.default_archive.is_some(),
            StockPatchSlot::ModernEffects => self.effects_cache.default_archive.is_some(),
            StockPatchSlot::LegacyEffects => self.effects_cache.legacy_archive.is_some(),
            StockPatchSlot::ModernEffectse => self.effectse_cache.default_archive.is_some(),
            StockPatchSlot::LegacyEffectse => self.effectse_cache.legacy_archive.is_some(),
            StockPatchSlot::ModernJunction => self.junction_cache.default_archive.is_some(),
            StockPatchSlot::LegacyJunction => self.junction_cache.legacy_archive.is_some(),
            StockPatchSlot::ModernWall(g) => self
                .wall_caches
                .get(&g)
                .is_some_and(|c| c.default_archive.is_some()),
            StockPatchSlot::LegacyWall(g) => self
                .wall_caches
                .get(&g)
                .is_some_and(|c| c.legacy_archive.is_some()),
        }
    }

    fn request_stock_patch(&mut self, slot: StockPatchSlot) {
        if !slot.is_valid() || self.has_stock_patch_loaded(slot) {
            return;
        }
        if self.pending_stock_patch_downloads.insert(slot) {
            let _ = self.to_net.send(UiToNet::DownloadStockPatch { slot });
        }
    }

    fn apply_stock_patch_downloaded(
        &mut self,
        slot: StockPatchSlot,
        archive: Option<Box<FoxArchive>>,
    ) {
        let Some(arc) = archive else {
            return;
        };
        let arc = *arc;
        self.cache_usage = None;
        match slot {
            StockPatchSlot::ModernFloors => self.floors_cache.set_default_archive(arc),
            StockPatchSlot::LegacyFloors => self.floors_cache.set_legacy_archive(arc),
            StockPatchSlot::LegacyFloors2 => self.floors2_cache.set_legacy_archive(arc),
            StockPatchSlot::ModernFloore => self.floore_cache.set_default_archive(arc),
            StockPatchSlot::LegacyFloore => self.floore_cache.set_legacy_archive(arc),
            StockPatchSlot::ModernItems => self.items_cache.set_default_archive(arc),
            StockPatchSlot::LegacyItem => self.items_cache.set_legacy_archive(arc),
            StockPatchSlot::LegacyItem2 => {
                if let Some((_, _, _, c)) = self
                    .legacy_item_splits
                    .iter_mut()
                    .find(|(min_id, _, _, _)| *min_id == 305)
                {
                    c.set_legacy_archive(arc);
                }
            }
            StockPatchSlot::LegacyItem3 => {
                if let Some((_, _, _, c)) = self
                    .legacy_item_splits
                    .iter_mut()
                    .find(|(min_id, _, _, _)| *min_id == 1000)
                {
                    c.set_legacy_archive(arc);
                }
            }
            StockPatchSlot::LegacyItem4 => {
                if let Some((_, _, _, c)) = self
                    .legacy_item_splits
                    .iter_mut()
                    .find(|(min_id, _, _, _)| *min_id == 1349)
                {
                    c.set_legacy_archive(arc);
                }
            }
            StockPatchSlot::ModernIteme => self.iteme_cache.set_default_archive(arc),
            StockPatchSlot::LegacyIteme => self.iteme_cache.set_legacy_archive(arc),
            StockPatchSlot::ModernPortals => self.portals_cache.set_default_archive(arc),
            StockPatchSlot::ModernPortalse => self.portalse_cache.set_default_archive(arc),
            StockPatchSlot::ModernEffects => self.effects_cache.set_default_archive(arc),
            StockPatchSlot::LegacyEffects => self.effects_cache.set_legacy_archive(arc),
            StockPatchSlot::ModernEffectse => self.effectse_cache.set_default_archive(arc),
            StockPatchSlot::LegacyEffectse => self.effectse_cache.set_legacy_archive(arc),
            StockPatchSlot::ModernJunction => self.junction_cache.set_default_archive(arc),
            StockPatchSlot::LegacyJunction => self.junction_cache.set_legacy_archive(arc),
            StockPatchSlot::ModernWall(g) => {
                let cache = self.wall_caches.entry(g).or_insert_with(|| FoxSpriteCache {
                    default_archive: None,
                    legacy_archive: None,
                    dream_archive: None,
                    shape_textures: HashMap::new(),
                    image_textures: HashMap::new(),
                    avatar_textures: HashMap::new(),
                    prefix: format!("wall_{g}"),
                });
                cache.set_default_archive(arc);
            }
            StockPatchSlot::LegacyWall(g) => {
                let cache = self.wall_caches.entry(g).or_insert_with(|| FoxSpriteCache {
                    default_archive: None,
                    legacy_archive: None,
                    dream_archive: None,
                    shape_textures: HashMap::new(),
                    image_textures: HashMap::new(),
                    avatar_textures: HashMap::new(),
                    prefix: format!("wall_{g}"),
                });
                cache.set_legacy_archive(arc);
            }
        }
    }

    fn reload_base_patch_caches(&mut self) {
        self.floors_cache
            .reload_base_from_assets(&self.assets, Some("floors.fox"), &["floors.fox"]);
        self.floors2_cache
            .reload_base_from_assets(&self.assets, None, &["floors2.fox"]);
        self.floore_cache
            .reload_base_from_assets(&self.assets, Some("floore.fox"), &["floore.fox"]);
        self.items_cache
            .reload_base_from_assets(&self.assets, Some("items.fox"), &["item.fox"]);
        self.iteme_cache
            .reload_base_from_assets(&self.assets, Some("iteme.fox"), &["iteme.fox"]);
        self.portals_cache
            .reload_base_from_assets(&self.assets, Some("portals.fox"), &[]);
        self.portalse_cache
            .reload_base_from_assets(&self.assets, Some("portalse.fox"), &[]);
        self.effects_cache
            .reload_base_from_assets(&self.assets, Some("effects.fox"), &["effects.fox"]);
        self.effectse_cache
            .reload_base_from_assets(&self.assets, Some("effectse.fox"), &["effectse.fox"]);
        self.lighting_cache
            .reload_base_from_assets(&self.assets, Some("lighting.fox"), &[]);
        self.lightinge_cache
            .reload_base_from_assets(&self.assets, Some("lightinge.fox"), &[]);
        self.ambience_cache
            .reload_base_from_assets(&self.assets, Some("ambience.fox"), &[]);
        self.ambiencee_cache
            .reload_base_from_assets(&self.assets, Some("ambiencee.fox"), &[]);
        self.junction_cache.reload_base_from_assets(
            &self.assets,
            Some("junction.fox"),
            &["junction.fsh", "junction.fox"],
        );
        for (min_id, _, _, cache) in &mut self.legacy_item_splits {
            let fname = match *min_id {
                305 => "item2.fox",
                1000 => "item3.fox",
                _ => "item4.fox",
            };
            cache.reload_base_from_assets(&self.assets, None, &[fname]);
        }
        if let Some(wall0) = self.wall_caches.get_mut(&0) {
            wall0.reload_base_from_assets(&self.assets, Some("wall.fox"), &["wall.fox"]);
        }
        for (g, cache) in &mut self.wall_caches {
            if *g == 0 {
                continue;
            }
            let fname = format!("wall_{g}.fox");
            cache.reload_base_from_assets(&self.assets, Some(&fname), &[&fname]);
        }
        for g in 1..=20usize {
            if self.wall_caches.contains_key(&g) {
                continue;
            }
            let fname = format!("wall_{g}.fox");
            if self.assets.default_patch_path(&fname).is_some()
                || self.assets.legacy_patch_path(&fname).is_some()
            {
                self.wall_caches.insert(
                    g,
                    FoxSpriteCache::load_with_legacy(
                        &self.assets,
                        &fname,
                        Some(&fname),
                        &format!("wall_{g}"),
                    ),
                );
            }
        }
    }

    fn dream_has_base_item(&self, item_id: u16) -> bool {
        if item_id == 0 {
            return true;
        }
        if !self.items_cache.has_dream() || item_id >= 1750 {
            for (min_id, max_id, offset, cache) in &self.dream_item_splits {
                if item_id >= *min_id && item_id <= *max_id {
                    let oid = item_id - *offset;
                    if cache
                        .dream_archive
                        .as_ref()
                        .is_some_and(|fox| fox.resolve_object_shape(oid, 0).is_some())
                    {
                        return true;
                    }
                }
            }
        }
        if let Some(fox) = self.items_cache.dream_archive.as_ref() {
            let dream_oid = if fox.is_fshx_or_fsh {
                item_id
            } else {
                item_id - 1
            };
            if fox.resolve_object_shape(dream_oid, 0).is_some() {
                return true;
            }
        }
        false
    }

    fn ensure_stock_patches_for_current_map(&mut self) {
        let is_modern = self.world.is_modern;
        let Some(map) = self.world.map.as_ref() else {
            self.request_stock_patch(if is_modern {
                StockPatchSlot::ModernFloors
            } else {
                StockPatchSlot::LegacyFloors
            });
            return;
        };

        let mut needed: Vec<StockPatchSlot> = Vec::new();
        let mut need_floors = false;
        let mut need_floors2 = false;
        let mut need_floore = false;
        let mut seen_floors = std::collections::HashSet::<u16>::new();

        for &f in &map.floors {
            if !seen_floors.insert(f) || f == 161 {
                continue;
            }
            if (209..=308).contains(&f) {
                if !need_floors2 {
                    let in_dream = self.floors2_cache.dream_archive.as_ref().is_some_and(|fox| {
                        fox.resolve_object_shape(f - 209, 0).is_some()
                            || fox.resolve_object_shape(f, 0).is_some()
                    });
                    if !in_dream {
                        need_floors2 = true;
                    }
                }
                continue;
            }
            if f >= 500 {
                if !need_floore {
                    let in_dream = self
                        .floore_cache
                        .dream_archive
                        .as_ref()
                        .is_some_and(|fox| fox.resolve_object_shape(f - 500, 0).is_some())
                        || self
                            .floors_cache
                            .dream_archive
                            .as_ref()
                            .is_some_and(|fox| fox.resolve_object_shape(f, 0).is_some());
                    if !in_dream {
                        need_floore = true;
                    }
                }
                continue;
            }
            if !need_floors {
                let in_dream = self
                    .floors_cache
                    .dream_archive
                    .as_ref()
                    .is_some_and(|fox| fox.resolve_object_shape(f, 0).is_some());
                if !in_dream {
                    need_floors = true;
                }
            }
        }
        if need_floors {
            needed.push(if is_modern {
                StockPatchSlot::ModernFloors
            } else {
                StockPatchSlot::LegacyFloors
            });
        }
        if need_floors2 {
            needed.push(StockPatchSlot::LegacyFloors2);
        }
        if need_floore {
            needed.push(if is_modern {
                StockPatchSlot::ModernFloore
            } else {
                StockPatchSlot::LegacyFloore
            });
        }

        let mut seen_items = std::collections::HashSet::<u16>::new();
        let mut need_modern_items = false;
        let mut need_legacy_item = false;
        let mut need_legacy_item2 = false;
        let mut need_legacy_item3 = false;
        let mut need_legacy_item4 = false;
        let mut need_iteme = false;
        let mut need_portals = is_modern
            && !self.world.dream_labels.is_empty()
            && self
                .portals_cache
                .dream_archive
                .as_ref()
                .is_none_or(|fox| !fox.has_portal_shapes(0));
        let mut need_portalse = false;

        for &it in &map.items {
            if it == 0 || !seen_items.insert(it) {
                continue;
            }
            if it >= 2400 {
                if !need_iteme
                    && self
                        .iteme_cache
                        .dream_archive
                        .as_ref()
                        .is_none_or(|fox| fox.resolve_object_shape(it - 2400, 0).is_none())
                {
                    need_iteme = true;
                }
                continue;
            }
            if (2000..=2099).contains(&it) {
                if is_modern && !need_portalse {
                    let pid = it - 2000;
                    let in_dream = self.portalse_cache.dream_archive.as_ref().is_some_and(|fox| {
                        fox.resolve_object_shape(pid, 0).is_some() || fox.has_portal_shapes(pid)
                    }) || self.portals_cache.dream_archive.as_ref().is_some_and(|fox| {
                        fox.resolve_object_shape(pid, 0).is_some() || fox.has_portal_shapes(pid)
                    });
                    if !in_dream {
                        need_portalse = true;
                    }
                }
                continue;
            }
            if (1800..=1899).contains(&it) {
                if is_modern && !need_portals {
                    let pid = it - 1800;
                    let in_dream = self.portals_cache.dream_archive.as_ref().is_some_and(|fox| {
                        fox.resolve_object_shape(pid, 0).is_some() || fox.has_portal_shapes(pid)
                    });
                    if !in_dream {
                        need_portals = true;
                    }
                }
                continue;
            }
            if is_modern {
                if it <= 1749 && !need_modern_items && !self.dream_has_base_item(it) {
                    need_modern_items = true;
                }
            } else if it <= 304 {
                if !need_legacy_item && !self.dream_has_base_item(it) {
                    need_legacy_item = true;
                }
            } else if it <= 999 {
                if !need_legacy_item2 && !self.dream_has_base_item(it) {
                    need_legacy_item2 = true;
                }
            } else if it <= 1348 {
                if !need_legacy_item3 && !self.dream_has_base_item(it) {
                    need_legacy_item3 = true;
                }
            } else if it <= 1748 && !need_legacy_item4 && !self.dream_has_base_item(it) {
                need_legacy_item4 = true;
            }
        }
        if need_modern_items {
            needed.push(StockPatchSlot::ModernItems);
        }
        if need_legacy_item {
            needed.push(StockPatchSlot::LegacyItem);
        }
        if need_legacy_item2 {
            needed.push(StockPatchSlot::LegacyItem2);
        }
        if need_legacy_item3 {
            needed.push(StockPatchSlot::LegacyItem3);
        }
        if need_legacy_item4 {
            needed.push(StockPatchSlot::LegacyItem4);
        }
        if need_iteme {
            needed.push(if is_modern {
                StockPatchSlot::ModernIteme
            } else {
                StockPatchSlot::LegacyIteme
            });
        }
        if need_portals {
            needed.push(StockPatchSlot::ModernPortals);
        }
        if need_portalse {
            needed.push(StockPatchSlot::ModernPortalse);
        }

        let mut wall_seen = [false; 256];
        let mut need_wall0 = false;
        let mut need_junction = false;
        let mut need_wall_groups = [false; 8];
        for &w in &map.walls {
            for b in [(w & 0xff) as usize, ((w >> 8) & 0xff) as usize] {
                if b == 0 || wall_seen[b] {
                    continue;
                }
                wall_seen[b] = true;
                let slot = b % 12;
                if slot < 4 && slot != 1 && slot != 3 {
                    continue;
                }
                let tex = b / 12;
                if tex < 2 {
                    let has_dream_group = self
                        .wall_caches
                        .get(&(tex + 1))
                        .is_some_and(|c| c.has_dream());
                    if !has_dream_group {
                        if !self.wall_caches.get(&0).is_some_and(|c| c.has_dream()) {
                            need_wall0 = true;
                        }
                        if !self.junction_cache.has_dream() {
                            need_junction = true;
                        }
                    }
                } else if (2..=6).contains(&tex) {
                    let g = tex + 1;
                    if !self.wall_caches.get(&g).is_some_and(|c| c.has_dream()) {
                        need_wall_groups[g] = true;
                    }
                }
            }
        }
        if need_wall0 {
            needed.push(if is_modern {
                StockPatchSlot::ModernWall(0)
            } else {
                StockPatchSlot::LegacyWall(0)
            });
        }
        if need_junction {
            needed.push(if is_modern {
                StockPatchSlot::ModernJunction
            } else {
                StockPatchSlot::LegacyJunction
            });
        }
        for (g, &wanted) in need_wall_groups.iter().enumerate() {
            if wanted {
                needed.push(if is_modern {
                    StockPatchSlot::ModernWall(g)
                } else {
                    StockPatchSlot::LegacyWall(g)
                });
            }
        }

        let mut seen_effects = std::collections::HashSet::<u16>::new();
        let mut need_effects = false;
        let mut need_effectse = false;
        for &e in &map.effects {
            if e == 0 || !seen_effects.insert(e) {
                continue;
            }
            if e >= 200 {
                if is_modern && e <= 249 && !need_effects && !self.effectse_cache.has_dream() {
                    let in_dream = self.effects_cache.dream_archive.as_ref().is_some_and(|fox| {
                        let oid = if fox.is_fshx_or_fsh { e } else { e - 1 };
                        fox.resolve_object_shape(oid, 0).is_some()
                    });
                    if !in_dream {
                        need_effects = true;
                    }
                }
                if !need_effectse
                    && self
                        .effectse_cache
                        .dream_archive
                        .as_ref()
                        .is_none_or(|fox| fox.resolve_object_shape(e - 200, 0).is_none())
                {
                    need_effectse = true;
                }
            } else if !need_effects {
                let in_dream = self.effects_cache.dream_archive.as_ref().is_some_and(|fox| {
                    let oid = if fox.is_fshx_or_fsh { e } else { e - 1 };
                    fox.resolve_object_shape(oid, 0).is_some()
                });
                if !in_dream {
                    need_effects = true;
                }
            }
        }
        if need_effects {
            needed.push(if is_modern {
                StockPatchSlot::ModernEffects
            } else {
                StockPatchSlot::LegacyEffects
            });
        }
        if need_effectse {
            needed.push(if is_modern {
                StockPatchSlot::ModernEffectse
            } else {
                StockPatchSlot::LegacyEffectse
            });
        }

        for slot in needed {
            self.request_stock_patch(slot);
        }
    }

    fn enter_dream(
        &mut self,
        name: &str,
        checksum: Option<&str>,
        explicit_url: Option<&str>,
        clear_avatars: bool,
        explicit_modern: Option<bool>,
    ) {
        let prev_name = self.world.map_name.as_deref().unwrap_or("");
        let switching = prev_name != name;
        if switching || clear_avatars {
            let preserved_avatars = if !clear_avatars {
                Some(std::mem::take(&mut self.world.avatars))
            } else {
                None
            };
            self.world.begin_dream(name.to_owned());
            if let Some(av) = preserved_avatars {
                self.world.avatars = av;
            }
        } else {
            self.world.map_name = Some(name.to_owned());
        }
        if let Some(m) = explicit_modern {
            self.world.is_modern = m;
        }
        self.pending_dream_modern = explicit_modern;
        self.pending_dream_name = Some(name.to_owned());
        self.pending_dream_received_bytes = 0;
        self.pending_dream_total_bytes = None;
        self.pending_dream_stage = "Connecting to dream server...".to_owned();
        if clear_avatars || switching {
            self.cam_offset = Vec2::ZERO;
        }
        self.clear_dream_patches();
        self.last_dream_id = Some(name.to_owned());
        self.last_dream_explicit_url = explicit_url.map(str::to_owned);

        if explicit_url.is_none() {
            if let Some((map, dir)) = self.try_load_cached_dream(name, checksum) {
                self.pending_dream_name = None;
                self.pending_dream_received_bytes = 0;
                self.pending_dream_total_bytes = None;
                self.pending_dream_stage.clear();
                self.load_dream_dir(&dir);
                let prev_modern = self
                    .pending_dream_modern
                    .take()
                    .unwrap_or(self.world.is_modern);
                self.world.attach_map(map);
                if prev_modern {
                    self.world.is_modern = true;
                }
                self.ensure_stock_patches_for_current_map();
                self.send_pending_vascodagama();
                return;
            }
        }

        // Fall back to any bundled local map temporarily while the live dream downloads.
        if switching || self.world.map.is_none() {
            self.world.map = self
                .assets
                .resolve_map_path(name)
                .and_then(|p| FurcadiaMap::load(&p).ok());
        }

        if self.screen == AppScreen::Connecting {
            self.connecting_stage = format!("Downloading dream '{name}'...");
        }
        let encoded_name = percent_encode_query(name);
        let url = explicit_url.map(str::to_owned).unwrap_or_else(|| {
            format!("http://apollo.furcadia.com/dream/get.php?file={encoded_name}&server=l")
        });
        let _ = self.to_net.send(UiToNet::DownloadDream {
            url,
            map_name: name.to_owned(),
            checksum: checksum.map(str::to_owned),
        });
    }

    fn resolve_floor_sprite(
        &mut self,
        ctx: &egui::Context,
        floor_id: u16,
        x: usize,
        y: usize,
    ) -> Option<CachedSprite> {
        let dream_overrides_floor = floor_id < 500
            && self
                .floors_cache
                .dream_archive
                .as_ref()
                .is_some_and(|f| f.resolve_shape(floor_id, 0).is_some());
        let fid = if dream_overrides_floor {
            floor_id
        } else {
            resolve_floor_variant(floor_id, x, y)
        };
        let use_legacy = !self.world.is_modern;
        // Map IDs 209..=308 are the floors2 namespace even in modern maps.
        // Keep consulting floors2 first so old background tiles (such as Vinca's
        // cloud floor 209) don't resolve to unrelated shapes in modern floors.fox.
        if (209..=308).contains(&fid) {
            if let Some(sp) = self
                .floors2_cache
                .get_or_load_shape_mode(ctx, &self.palette, fid - 209, 0, use_legacy)
                .or_else(|| {
                    self.floors2_cache
                        .get_or_load_shape_mode(ctx, &self.palette, fid, 0, use_legacy)
                })
            {
                return Some(sp);
            }
            if !self.floors2_cache.has_dream() && self.floors2_cache.legacy_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::LegacyFloors2);
            }
        }
        if fid >= 500 {
            let res = self
                .floore_cache
                .get_or_load_shape_mode(ctx, &self.palette, fid - 500, 0, use_legacy)
                .or_else(|| {
                    self.floors_cache
                        .get_or_load_shape_mode(ctx, &self.palette, fid, 0, use_legacy)
                });
            if res.is_none() && !self.floore_cache.has_dream() {
                if use_legacy && self.floore_cache.legacy_archive.is_none() {
                    self.request_stock_patch(StockPatchSlot::LegacyFloore);
                } else if !use_legacy && self.floore_cache.default_archive.is_none() {
                    self.request_stock_patch(StockPatchSlot::ModernFloore);
                }
            }
            return res;
        }
        let res = self
            .floors_cache
            .get_or_load_shape_mode(ctx, &self.palette, fid, 0, use_legacy);
        if res.is_none() && floor_id != 161 {
            if use_legacy && self.floors_cache.legacy_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::LegacyFloors);
            } else if !use_legacy && self.floors_cache.default_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::ModernFloors);
            }
        }
        res
    }

    fn resolve_floor_layers(
        &mut self,
        ctx: &egui::Context,
        floor_id: u16,
        x: usize,
        y: usize,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        let dream_overrides_floor = floor_id < 500
            && self
                .floors_cache
                .dream_archive
                .as_ref()
                .is_some_and(|fox| fox.resolve_object_shape(floor_id, 0).is_some());
        let fid = if dream_overrides_floor {
            floor_id
        } else {
            resolve_floor_variant(floor_id, x, y)
        };
        let use_legacy = !self.world.is_modern;

        // Map IDs 209..=308 are the floors2 namespace even in modern maps.
        if (209..=308).contains(&fid) {
            if let Some(shape) = self
                .floors2_cache
                .get_or_load_shape_layers_mode(ctx, &self.palette, fid - 209, 0, anim_ms, use_legacy)
                .or_else(|| {
                    self.floors2_cache.get_or_load_shape_layers_mode(
                        ctx,
                        &self.palette,
                        fid,
                        0,
                        anim_ms,
                        use_legacy,
                    )
                })
            {
                return Some(shape);
            }
            if !self.floors2_cache.has_dream() && self.floors2_cache.legacy_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::LegacyFloors2);
            }
        }

        if fid >= 500 {
            let res = self
                .floore_cache
                .get_or_load_shape_layers_mode(
                    ctx,
                    &self.palette,
                    fid - 500,
                    0,
                    anim_ms,
                    use_legacy,
                )
                .or_else(|| {
                    self.floors_cache.get_or_load_shape_layers_mode(
                        ctx,
                        &self.palette,
                        fid,
                        0,
                        anim_ms,
                        use_legacy,
                    )
                });
            if res.is_none() && !self.floore_cache.has_dream() {
                if use_legacy && self.floore_cache.legacy_archive.is_none() {
                    self.request_stock_patch(StockPatchSlot::LegacyFloore);
                } else if !use_legacy && self.floore_cache.default_archive.is_none() {
                    self.request_stock_patch(StockPatchSlot::ModernFloore);
                }
            }
            return res;
        }

        let res = self
            .floors_cache
            .get_or_load_shape_layers_mode(ctx, &self.palette, fid, 0, anim_ms, use_legacy);
        if res.is_none() && floor_id != 161 {
            if use_legacy && self.floors_cache.legacy_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::LegacyFloors);
            } else if !use_legacy && self.floors_cache.default_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::ModernFloors);
            }
        }
        res
    }

    fn resolve_item_layers(
        &mut self,
        ctx: &egui::Context,
        item_id: u16,
        portal_state: Option<i32>,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        if item_id == 0 {
            if self.world.is_modern && portal_state.is_some() {
                let res = resolve_portal_layers_from(
                    &mut self.portals_cache,
                    ctx,
                    &self.palette,
                    0,
                    portal_state,
                    anim_ms,
                );
                if res.is_none()
                    && !self.portals_cache.has_dream()
                    && self.portals_cache.default_archive.is_none()
                {
                    self.request_stock_patch(StockPatchSlot::ModernPortals);
                }
                return res;
            }
            return None;
        }
        if (1800..=1899).contains(&item_id) {
            let pid = item_id - 1800;
            let res = resolve_portal_layers_from(
                &mut self.portals_cache,
                ctx,
                &self.palette,
                pid,
                portal_state,
                anim_ms,
            )
            .or_else(|| {
                if portal_state.is_some_and(|s| s >= 0) && pid != 0 {
                    resolve_portal_layers_from(
                        &mut self.portals_cache,
                        ctx,
                        &self.palette,
                        0,
                        portal_state,
                        anim_ms,
                    )
                } else {
                    None
                }
            });
            if res.is_none()
                && !self.portals_cache.has_dream()
                && self.portals_cache.default_archive.is_none()
            {
                self.request_stock_patch(StockPatchSlot::ModernPortals);
            }
            return res;
        }
        if (2000..=2099).contains(&item_id) {
            let pid = item_id - 2000;
            let res = resolve_portal_layers_from(
                &mut self.portalse_cache,
                ctx,
                &self.palette,
                pid,
                portal_state,
                anim_ms,
            )
            .or_else(|| {
                resolve_portal_layers_from(
                    &mut self.portals_cache,
                    ctx,
                    &self.palette,
                    pid,
                    portal_state,
                    anim_ms,
                )
            })
            .or_else(|| {
                if portal_state.is_some_and(|s| s >= 0) && pid != 0 {
                    resolve_portal_layers_from(
                        &mut self.portals_cache,
                        ctx,
                        &self.palette,
                        0,
                        portal_state,
                        anim_ms,
                    )
                } else {
                    None
                }
            });
            if res.is_none() {
                if !self.portalse_cache.has_dream() && self.portalse_cache.default_archive.is_none()
                {
                    self.request_stock_patch(StockPatchSlot::ModernPortalse);
                }
                if !self.portals_cache.has_dream() && self.portals_cache.default_archive.is_none() {
                    self.request_stock_patch(StockPatchSlot::ModernPortals);
                }
            }
            return res;
        }
        let use_legacy = !self.world.is_modern;
        if item_id >= 2400 {
            let oid = item_id - 2400;
            if self.world.is_modern
                && portal_state.is_some_and(|s| s >= 0)
                && cache_has_portal_shapes(&self.iteme_cache, oid)
            {
                if let res @ Some(_) = resolve_portal_layers_from(
                    &mut self.iteme_cache,
                    ctx,
                    &self.palette,
                    oid,
                    portal_state,
                    anim_ms,
                ) {
                    return res;
                }
                return resolve_portal_layers_from(
                    &mut self.portals_cache,
                    ctx,
                    &self.palette,
                    0,
                    portal_state,
                    anim_ms,
                );
            }
            if let res @ Some(_) = self.iteme_cache.get_or_load_shape_layers_mode(
                ctx,
                &self.palette,
                oid,
                0,
                anim_ms,
                use_legacy,
            ) {
                return res;
            }
            if use_legacy && !self.iteme_cache.has_dream() && self.iteme_cache.legacy_archive.is_none()
            {
                self.request_stock_patch(StockPatchSlot::LegacyIteme);
            } else if !use_legacy
                && !self.iteme_cache.has_dream()
                && self.iteme_cache.default_archive.is_none()
            {
                self.request_stock_patch(StockPatchSlot::ModernIteme);
            }
        }
        if !self.items_cache.has_dream() || item_id >= 1750 {
            for (min_id, max_id, offset, cache) in &mut self.dream_item_splits {
                if item_id >= *min_id && item_id <= *max_id {
                    let oid = item_id - *offset;
                    if self.world.is_modern
                        && portal_state.is_some_and(|s| s >= 0)
                        && cache_has_portal_shapes(cache, oid)
                    {
                        if let res @ Some(_) = resolve_portal_layers_from(
                            cache,
                            ctx,
                            &self.palette,
                            oid,
                            portal_state,
                            anim_ms,
                        ) {
                            return res;
                        }
                        return resolve_portal_layers_from(
                            &mut self.portals_cache,
                            ctx,
                            &self.palette,
                            0,
                            portal_state,
                            anim_ms,
                        );
                    }
                    if let res @ Some(_) =
                        cache.get_or_load_shape_layers(ctx, &self.palette, oid, 0, anim_ms)
                    {
                        return res;
                    }
                }
            }
        }
        if self.items_cache.has_dream() {
            let dream_oid = if self.items_cache.dream_is_fshx_or_fsh() {
                item_id
            } else {
                item_id - 1
            };
            if self.world.is_modern
                && portal_state.is_some_and(|s| s >= 0)
                && cache_has_portal_shapes(&self.items_cache, dream_oid)
            {
                if let res @ Some(_) = resolve_portal_layers_from(
                    &mut self.items_cache,
                    ctx,
                    &self.palette,
                    dream_oid,
                    portal_state,
                    anim_ms,
                ) {
                    return res;
                }
                return resolve_portal_layers_from(
                    &mut self.portals_cache,
                    ctx,
                    &self.palette,
                    0,
                    portal_state,
                    anim_ms,
                );
            }
            if let res @ Some(_) = self.items_cache.get_or_load_dream_shape_layers(
                ctx,
                &self.palette,
                dream_oid,
                0,
                anim_ms,
            ) {
                return res;
            }
        }

        let oid = item_id - 1;
        if use_legacy {
            let mut missing_split_slot: Option<StockPatchSlot> = None;
            if (305..=1748).contains(&item_id) {
                for (min_id, max_id, offset, cache) in &mut self.legacy_item_splits {
                    if item_id >= *min_id && item_id <= *max_id {
                        let legacy_oid = item_id - *offset;
                        if let res @ Some(_) = cache.get_or_load_legacy_shape_layers(
                            ctx,
                            &self.palette,
                            legacy_oid,
                            0,
                            anim_ms,
                        ) {
                            return res;
                        }
                        if cache.legacy_archive.is_none() {
                            missing_split_slot = Some(match *min_id {
                                305 => StockPatchSlot::LegacyItem2,
                                1000 => StockPatchSlot::LegacyItem3,
                                _ => StockPatchSlot::LegacyItem4,
                            });
                        }
                    }
                }
            }
            if let res @ Some(_) = self.items_cache.get_or_load_legacy_shape_layers(
                ctx,
                &self.palette,
                item_id,
                0,
                anim_ms,
            ) {
                return res;
            }
            if let Some(slot) = missing_split_slot {
                self.request_stock_patch(slot);
            } else if item_id <= 304 && self.items_cache.legacy_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::LegacyItem);
            }
            if let res @ Some(_) = self.items_cache.get_or_load_default_shape_layers(
                ctx,
                &self.palette,
                oid,
                0,
                anim_ms,
            ) {
                return res;
            }
            return None;
        }

        if portal_state.is_some_and(|s| s >= 0) && cache_has_portal_shapes(&self.items_cache, oid) {
            if let res @ Some(_) = resolve_portal_layers_from(
                &mut self.items_cache,
                ctx,
                &self.palette,
                oid,
                portal_state,
                anim_ms,
            ) {
                return res;
            }
            return resolve_portal_layers_from(
                &mut self.portals_cache,
                ctx,
                &self.palette,
                0,
                portal_state,
                anim_ms,
            );
        }
        if let res @ Some(_) = self.items_cache.get_or_load_default_shape_layers(
            ctx,
            &self.palette,
            oid,
            0,
            anim_ms,
        ) {
            return res;
        }
        if (1..=1749).contains(&item_id) && self.items_cache.default_archive.is_none() {
            self.request_stock_patch(StockPatchSlot::ModernItems);
        }
        if (305..=1748).contains(&item_id) {
            for (min_id, max_id, offset, cache) in &mut self.legacy_item_splits {
                if item_id >= *min_id && item_id <= *max_id {
                    let legacy_oid = item_id - *offset;
                    if let res @ Some(_) = cache.get_or_load_legacy_shape_layers(
                        ctx,
                        &self.palette,
                        legacy_oid,
                        0,
                        anim_ms,
                    ) {
                        return res;
                    }
                }
            }
        }
        if let res @ Some(_) = self.items_cache.get_or_load_legacy_shape_layers(
            ctx,
            &self.palette,
            item_id,
            0,
            anim_ms,
        ) {
            return res;
        }
        if portal_state.is_some_and(|s| s >= 0) {
            let res = resolve_portal_layers_from(
                &mut self.portals_cache,
                ctx,
                &self.palette,
                0,
                portal_state,
                anim_ms,
            );
            if res.is_none()
                && !self.portals_cache.has_dream()
                && self.portals_cache.default_archive.is_none()
            {
                self.request_stock_patch(StockPatchSlot::ModernPortals);
            }
            return res;
        }
        None
    }

    fn resolve_effect_layers(
        &mut self,
        ctx: &egui::Context,
        effect_id: u16,
        anim_ms: u64,
    ) -> Option<ResolvedShapeLayers> {
        if effect_id == 0 {
            return None;
        }
        let use_legacy = !self.world.is_modern;
        let resolve_effects_base = |app: &mut Self, id: u16| -> Option<ResolvedShapeLayers> {
            if app.effects_cache.has_dream() && app.effects_cache.dream_is_fshx_or_fsh() {
                if let res @ Some(_) = app.effects_cache.get_or_load_dream_shape_layers(
                    ctx,
                    &app.palette,
                    id,
                    0,
                    anim_ms,
                ) {
                    return res;
                }
            }
            app.effects_cache.get_or_load_shape_layers_mode(
                ctx,
                &app.palette,
                id - 1,
                0,
                anim_ms,
                use_legacy,
            )
        };
        if effect_id >= 200 {
            if effect_id <= 249 && !self.effectse_cache.has_dream() {
                if let res @ Some(_) = resolve_effects_base(self, effect_id) {
                    return res;
                }
            }
            if let res @ Some(_) = self.effectse_cache.get_or_load_shape_layers_mode(
                ctx,
                &self.palette,
                effect_id - 200,
                0,
                anim_ms,
                use_legacy,
            ) {
                return res;
            }
            if effect_id <= 249 {
                if let res @ Some(_) = resolve_effects_base(self, effect_id) {
                    return res;
                }
                if !use_legacy
                    && !self.effects_cache.has_dream()
                    && self.effects_cache.default_archive.is_none()
                {
                    self.request_stock_patch(StockPatchSlot::ModernEffects);
                }
            }
            if !self.effectse_cache.has_dream() {
                if use_legacy && self.effectse_cache.legacy_archive.is_none() {
                    self.request_stock_patch(StockPatchSlot::LegacyEffectse);
                } else if !use_legacy && self.effectse_cache.default_archive.is_none() {
                    self.request_stock_patch(StockPatchSlot::ModernEffectse);
                }
            }
            return None;
        }
        let res = resolve_effects_base(self, effect_id);
        if res.is_none() && !self.effects_cache.has_dream() {
            if use_legacy && self.effects_cache.legacy_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::LegacyEffects);
            } else if !use_legacy && self.effects_cache.default_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::ModernEffects);
            }
        }
        res
    }

    fn resolve_junction_sprite(
        &mut self,
        ctx: &egui::Context,
        group_or: usize,
        mask: u8,
    ) -> Option<CachedSprite> {
        if group_or > 1 || mask == 0 || mask == 6 || mask == 9 || mask > 15 {
            return None;
        }
        let use_legacy = !self.world.is_modern;
        // Dream wall_1.fox / wall_2.fox suppresses junction.fox for that group (`ps.iv`).
        if self.wall_caches.get(&(group_or + 1)).is_some_and(|c| {
            c.has_dream() || c.default_archive.is_some() || c.legacy_archive.is_some()
        }) {
            return None;
        }
        let obj_id = (15 * group_or + mask as usize) as u16;
        let is_fshx = self.junction_cache.active_is_fshx_or_fsh(use_legacy);
        let dir = if is_fshx { 0 } else { 7 };
        let res = self
            .junction_cache
            .get_or_load_shape_mode(ctx, &self.palette, obj_id, dir, use_legacy);
        if res.is_none() && !self.junction_cache.has_dream() {
            if use_legacy && self.junction_cache.legacy_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::LegacyJunction);
            } else if !use_legacy && self.junction_cache.default_archive.is_none() {
                self.request_stock_patch(StockPatchSlot::ModernJunction);
            }
        }
        res
    }

    fn resolve_wall_edge_sprite(
        &mut self,
        ctx: &egui::Context,
        wall_byte: u16,
        dir: u8,
    ) -> Option<CachedSprite> {
        let tex = (wall_byte / 12) as usize;
        let v = wall_byte % 12;
        if wall_byte == 0 || tex >= 18 {
            return None;
        }
        let obj: u16 = if v >= 4 {
            v - 4
        } else if v == 1 || v == 3 {
            8
        } else {
            return None;
        };

        let use_legacy = !self.world.is_modern;
        let has_tex_file = self.wall_caches.get(&(tex + 1)).is_some_and(|c| {
            c.has_dream() || c.default_archive.is_some() || c.legacy_archive.is_some()
        });

        if tex < 2 && !has_tex_file {
            if obj == 8 {
                let junc_obj = if dir == 7 {
                    1 + 15 * (tex as u16) + 8
                } else {
                    1 + 15 * (tex as u16) + 5
                };
                let jdir = if self.junction_cache.active_is_fshx_or_fsh(use_legacy) {
                    0
                } else {
                    7
                };
                let res = self.junction_cache.get_or_load_shape_mode(
                    ctx,
                    &self.palette,
                    junc_obj,
                    jdir,
                    use_legacy,
                );
                if res.is_none() && !self.junction_cache.has_dream() {
                    if use_legacy && self.junction_cache.legacy_archive.is_none() {
                        self.request_stock_patch(StockPatchSlot::LegacyJunction);
                    } else if !use_legacy && self.junction_cache.default_archive.is_none() {
                        self.request_stock_patch(StockPatchSlot::ModernJunction);
                    }
                }
                return res;
            }
            let (res, missing_base) = {
                let Some(cache) = self.wall_caches.get_mut(&0) else {
                    self.request_stock_patch(if use_legacy {
                        StockPatchSlot::LegacyWall(0)
                    } else {
                        StockPatchSlot::ModernWall(0)
                    });
                    return None;
                };
                let is_paired = cache.active_uses_paired_wall_ids(use_legacy);
                let side_off = if dir == 9 { 1 } else { 0 };
                let (obj_id, lookup_dir) = if is_paired {
                    (16 * tex as u16 + 2 * obj + side_off, 0)
                } else {
                    (8 * tex as u16 + obj, dir)
                };
                if cache.has_dream() {
                    (
                        cache.get_or_load_dream_shape(ctx, &self.palette, obj_id, lookup_dir),
                        false,
                    )
                } else {
                    let loaded = cache.get_or_load_shape_mode(
                        ctx,
                        &self.palette,
                        obj_id,
                        lookup_dir,
                        use_legacy,
                    );
                    let need_base = loaded.is_none()
                        && if use_legacy {
                            cache.legacy_archive.is_none()
                        } else {
                            cache.default_archive.is_none()
                        };
                    (loaded, need_base)
                }
            };
            if missing_base {
                self.request_stock_patch(if use_legacy {
                    StockPatchSlot::LegacyWall(0)
                } else {
                    StockPatchSlot::ModernWall(0)
                });
            }
            return res;
        }

        let file_key = tex + 1;
        let (res, missing_base) = {
            let Some(cache) = self.wall_caches.get_mut(&file_key) else {
                if (3..=7).contains(&file_key) {
                    self.request_stock_patch(if use_legacy {
                        StockPatchSlot::LegacyWall(file_key)
                    } else {
                        StockPatchSlot::ModernWall(file_key)
                    });
                }
                return None;
            };
            let is_paired = cache.active_uses_paired_wall_ids(use_legacy);
            let side_off = if dir == 9 { 1 } else { 0 };
            let (obj_id, lookup_dir) = if is_paired {
                (2 * obj + side_off, 0)
            } else {
                (obj, dir)
            };
            if cache.has_dream() {
                (
                    cache.get_or_load_dream_shape(ctx, &self.palette, obj_id, lookup_dir),
                    false,
                )
            } else {
                let loaded = cache.get_or_load_shape_mode(
                    ctx,
                    &self.palette,
                    obj_id,
                    lookup_dir,
                    use_legacy,
                );
                let need_base = loaded.is_none()
                    && (3..=7).contains(&file_key)
                    && if use_legacy {
                        cache.legacy_archive.is_none()
                    } else {
                        cache.default_archive.is_none()
                    };
                (loaded, need_base)
            }
        };
        if missing_base {
            self.request_stock_patch(if use_legacy {
                StockPatchSlot::LegacyWall(file_key)
            } else {
                StockPatchSlot::ModernWall(file_key)
            });
        }
        res
    }

    fn handle_net_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.from_net.try_recv() {
            match ev {
                NetToUi::Status(s) => self.status_text = s,
                NetToUi::AuthError(err) => {
                    self.auth_busy = false;
                    self.auth_error = Some(err);
                }
                NetToUi::Authenticated(info) => {
                    self.auth_busy = false;
                    self.auth_error = None;
                    let mut config_updated = false;
                    for ch in &info.characters {
                        let key = shortname_key(&ch.name);
                        if key.is_empty() {
                            continue;
                        }
                        if let Some(pid) = ch.last_port.filter(|&p| p > 16) {
                            if self.player_portrait_ids.insert(key.clone(), pid) != Some(pid) {
                                config_updated = true;
                            }
                        }
                        if let Some(pid) =
                            resolve_custom_portrait_id(&self.player_portrait_ids, &key, ch.last_port)
                        {
                            if !self.custom_portrait_rgba.contains_key(&pid)
                                && self.pending_portrait_downloads.insert(pid)
                            {
                                let _ = self
                                    .to_net
                                    .send(UiToNet::DownloadPortrait { portrait_id: pid });
                            }
                        }
                    }
                    let saved_norm = self.saved_character.trim().replace('|', " ");
                    self.selected_char_idx = if saved_norm.is_empty() {
                        0
                    } else {
                        info.characters
                            .iter()
                            .position(|c| {
                                c.name.eq_ignore_ascii_case(&self.saved_character)
                                    || c.name.replace('|', " ").eq_ignore_ascii_case(&saved_norm)
                            })
                            .unwrap_or(0)
                    };
                    if let Some(ch) = info.characters.get(self.selected_char_idx) {
                        self.edit_desc_buf = ch.desc.clone();
                    }
                    self.account_info = Some(info);
                    if config_updated {
                        self.save_config();
                    }
                    self.screen = AppScreen::CharacterSelect;
                }
                NetToUi::News(entries) => {
                    self.news = entries;
                }
                NetToUi::ConnectStage { seq, stage } => {
                    if seq == self.connect_seq {
                        self.connecting_stage = stage;
                    }
                }
                NetToUi::ConnectFailed { seq, error } => {
                    if seq == self.connect_seq {
                        self.is_connected = false;
                        self.reconnecting = false;
                        self.connected_at = None;
                        self.connecting_error = Some(error.clone());
                        self.connecting_stage = "Connection failed".to_owned();
                        self.status_text = error.clone();
                        self.push_system_line("error", &error);
                    }
                }
                NetToUi::Connected { seq, connected } => {
                    if seq == self.connect_seq {
                        self.is_connected = connected;
                        self.reconnecting = false;
                        if self.chat_pinned_to_bottom {
                            self.chat_scroll_to_bottom = true;
                        }
                        if connected {
                            self.connecting_error = None;
                            self.connected_at = Some(Instant::now());
                            self.connecting_stage = "Logged in! Entering dream...".to_owned();
                        } else {
                            self.connected_at = None;
                            if self.screen == AppScreen::Connecting && self.connecting_error.is_none()
                            {
                                self.connecting_error =
                                    Some("Disconnected by server.".to_owned());
                            } else if self.screen == AppScreen::InGame {
                                self.push_system_line("warning", "Disconnected from server.");
                            }
                        }
                    }
                }
                NetToUi::Message(msg) => self.process_server_message(msg),
                NetToUi::DreamDownloadProgress {
                    map_name,
                    received_bytes,
                    total_bytes,
                    stage,
                } => {
                    if let Some(expected) = self.pending_dream_name.as_deref() {
                        let e = expected.trim().trim_end_matches(".map");
                        let m = map_name.trim().trim_end_matches(".map");
                        if !e.is_empty() && !e.eq_ignore_ascii_case(m) {
                            continue;
                        }
                    }
                    self.pending_dream_received_bytes = received_bytes;
                    if total_bytes.is_some() {
                        self.pending_dream_total_bytes = total_bytes;
                    }
                    self.pending_dream_stage = stage;
                    if self.screen == AppScreen::Connecting {
                        self.connecting_stage = match self.pending_dream_total_bytes {
                            Some(total) if total > 0 => format!(
                                "Downloading dream '{map_name}' ({} / {})...",
                                format_bytes(received_bytes),
                                format_bytes(total)
                            ),
                            _ if received_bytes > 0 => format!(
                                "Downloading dream '{map_name}' ({})...",
                                format_bytes(received_bytes)
                            ),
                            _ => format!("Downloading dream '{map_name}'..."),
                        };
                    }
                }
                NetToUi::DreamDownloaded {
                    map_name,
                    path,
                    dream_dir,
                } => {
                    if let Some(expected) = self.pending_dream_name.as_deref() {
                        let e = expected.trim().trim_end_matches(".map");
                        let m = map_name.trim().trim_end_matches(".map");
                        if !e.is_empty() && !e.eq_ignore_ascii_case(m) {
                            continue;
                        }
                    }
                    self.pending_dream_name = None;
                    self.pending_dream_received_bytes = 0;
                    self.pending_dream_total_bytes = None;
                    self.pending_dream_stage.clear();
                    self.cache_usage = None;
                    self.clear_dream_patches();
                    if let Some(dir) = dream_dir {
                        self.load_dream_dir(&dir);
                    }
                    match FurcadiaMap::load(&path) {
                        Ok(map) => {
                            self.world.map_name = Some(map_name.clone());
                            let prev_modern = self
                                .pending_dream_modern
                                .take()
                                .unwrap_or(self.world.is_modern);
                            self.world.attach_map(map);
                            if prev_modern {
                                self.world.is_modern = true;
                            }
                            self.ensure_stock_patches_for_current_map();
                            self.cam_offset = Vec2::ZERO;
                            self.push_system_line(
                                "success",
                                &format!("Loaded dream package '{map_name}'."),
                            );
                            self.send_pending_vascodagama();
                        }
                        Err(e) => {
                            self.push_system_line(
                                "warning",
                                &format!("Failed to parse dream map '{map_name}': {e}"),
                            );
                            self.send_pending_vascodagama();
                        }
                    }
                }
                NetToUi::DreamError(msg) => {
                    self.pending_dream_name = None;
                    self.pending_dream_received_bytes = 0;
                    self.pending_dream_total_bytes = None;
                    self.pending_dream_stage.clear();
                    self.push_system_line("warning", &msg);
                    if self.screen == AppScreen::Connecting && self.is_connected {
                        self.screen = AppScreen::InGame;
                    }
                    if !self.world.map_ready {
                        if let Some(fallback_map) = self.world.map.take() {
                            let prev_modern = self
                                .pending_dream_modern
                                .take()
                                .unwrap_or(self.world.is_modern);
                            self.world.attach_map(fallback_map);
                            if prev_modern {
                                self.world.is_modern = true;
                            }
                            self.ensure_stock_patches_for_current_map();
                        }
                    }
                    self.send_pending_vascodagama();
                }
                NetToUi::StockPatchDownloaded { slot, archive } => {
                    self.apply_stock_patch_downloaded(slot, archive);
                }
                NetToUi::SpeciesDownloaded { file_id, path } => {
                    if let Some(p) = path {
                        self.cache_usage = None;
                        let prefix = if file_id >= DYNAMIC_AVATAR_CACHE_BASE {
                            format!("dplayer_{}", file_id - DYNAMIC_AVATAR_CACHE_BASE)
                        } else if file_id > 1000 {
                            format!("local_{}", file_id - 1000)
                        } else {
                            format!("species_{file_id}")
                        };
                        let mut cache = FoxSpriteCache::from_dream_only(&p, &prefix);
                        // Promote the downloaded file to default_archive so dream patches can override it.
                        cache.default_archive = cache.dream_archive.take();
                        if file_id < DYNAMIC_AVATAR_CACHE_BASE {
                            if let Some(dir) = self.current_dream_dir.as_deref() {
                                let patch_p = if file_id > 1000 {
                                    Self::find_file_case_insensitive(
                                        dir,
                                        &format!("species{file_id}.fox"),
                                    )
                                    .or_else(|| {
                                        Self::find_file_case_insensitive(
                                            dir,
                                            &format!("local{}.fox", file_id - 1000),
                                        )
                                    })
                                } else {
                                    Self::find_file_case_insensitive(
                                        dir,
                                        &format!("species{file_id}.fox"),
                                    )
                                };
                                if let Some(patch_p) = patch_p {
                                    let _ = cache.set_dream_patch(&patch_p);
                                }
                            }
                        }
                        if cache.is_loaded() {
                            self.species_caches.insert(file_id, cache);
                            self.char_portraits.clear();
                            self.invalidate_looked_card_textures();
                        }
                    }
                }
                NetToUi::PortraitDownloaded { portrait_id, rgba, remappable } => {
                    self.apply_portrait_downloaded(portrait_id, rgba, remappable);
                }
                NetToUi::ChatImageDownloaded { url, rgba } => {
                    self.pending_chat_img_downloads.remove(&url);
                    self.cache_usage = None;
                    let tex = rgba.map(|(w, h, bytes)| {
                        let img = ColorImage::from_rgba_unmultiplied([w, h], &bytes);
                        ctx.load_texture(format!("chat_img_{url}"), img, TextureOptions::NEAREST)
                    });
                    if self.chat_img_textures.len() >= MAX_CHAT_IMG_TEXTURES {
                        self.chat_img_textures.clear();
                    }
                    self.chat_img_textures.insert(url, tex);
                }
                NetToUi::SpellCheckResult { seq, misspelled } => {
                    if seq == self.spell_seq {
                        self.spell_misspelled = misspelled
                            .into_iter()
                            .filter(|m| !self.custom_dict_words.contains(&m.word.to_ascii_lowercase()))
                            .collect();
                    }
                }
            }
        }
    }

    fn save_active_chat_draft(&mut self) {
        if let Some(peer) = self.active_chat_tab.as_deref() {
            if let Some(whisper) = self
                .whispers
                .iter_mut()
                .find(|w| w.peer.eq_ignore_ascii_case(peer))
            {
                whisper.input.clone_from(&self.chat_input);
            }
        } else {
            self.main_chat_draft.clone_from(&self.chat_input);
        }
    }

    fn track_pending_whisper_echo(&mut self, target: &str, message: &str) {
        let now = Instant::now();
        let max_age = std::time::Duration::from_secs(30);
        self.pending_whisper_echoes
            .retain(|(_, _, sent_at)| now.duration_since(*sent_at) < max_age);
        self.pending_whisper_echoes.push((
            target.replace('|', " "),
            normalize_whisper_body(message),
            now,
        ));
        if self.pending_whisper_echoes.len() > 16 {
            self.pending_whisper_echoes.remove(0);
        }
    }

    fn load_active_chat_draft(&mut self) {
        self.chat_input = self
            .active_chat_tab
            .as_deref()
            .and_then(|peer| {
                self.whispers
                    .iter()
                    .find(|w| w.peer.eq_ignore_ascii_case(peer))
                    .map(|w| w.input.clone())
            })
            .unwrap_or_else(|| self.main_chat_draft.clone());
    }

    fn switch_chat_tab(&mut self, target: Option<String>) {
        let same_tab = match (self.active_chat_tab.as_deref(), target.as_deref()) {
            (None, None) => true,
            (Some(active), Some(next)) => active.eq_ignore_ascii_case(next),
            _ => false,
        };
        if same_tab {
            if let Some(peer) = target.as_deref() {
                let current_key = self
                    .looked_player
                    .as_ref()
                    .map(|c| shortname_key(&c.short_name));
                if current_key.as_deref() != Some(shortname_key(peer).as_str()) {
                    self.sync_portrait_for_active_chat_tab();
                }
            }
            return;
        }
        if self.active_chat_tab.is_none() {
            self.main_looked_player = self.looked_player.clone();
        }
        self.save_active_chat_draft();
        self.active_chat_tab = target;
        self.load_active_chat_draft();
        self.chat_pinned_to_bottom = true;
        self.chat_scroll_to_bottom = true;
        self.spell_seq = self.spell_seq.wrapping_add(1);
        self.spell_dirty_at = None;
        self.spell_misspelled.clear();
        self.sync_portrait_for_active_chat_tab();
    }

    fn open_whisper_tab(&mut self, peer: &str, focus: bool) -> usize {
        let trimmed = peer.trim();
        if let Some(idx) = self
            .whispers
            .iter()
            .position(|w| w.peer.eq_ignore_ascii_case(trimmed))
        {
            if focus {
                self.whispers[idx].unread = 0;
                self.switch_chat_tab(Some(self.whispers[idx].peer.clone()));
                self.focus_chat_input = true;
            }
            return idx;
        }
        let mut session = WhisperSession::new(trimmed.to_owned());
        let wire_name = self.active_character_wire_name();
        session.history_lines = self.load_whisper_history_for(&wire_name, trimmed);
        self.whispers.push(session);
        let idx = self.whispers.len() - 1;
        if focus {
            self.switch_chat_tab(Some(self.whispers[idx].peer.clone()));
            self.focus_chat_input = true;
        }
        idx
    }

    fn process_server_message(&mut self, msg: ServerMessage) {
        let is_map_directive = matches!(
            &msg,
            ServerMessage::UseMap(_)
                | ServerMessage::MapName(_)
                | ServerMessage::Extended {
                    code: Some('q' | 'r' | 'J' | 'M' | '&'),
                    ..
                }
                | ServerMessage::ExtendedBytes {
                    code: b'q' | b'r' | b'J' | b'M' | b'&' | b'f',
                    ..
                }
        );
        if self.show_debug_packets
            && ((self.login_trace_remaining > 0 && !matches!(msg, ServerMessage::WorldPacket { .. }))
                || is_map_directive)
        {
            if self.login_trace_remaining > 0 && !matches!(msg, ServerMessage::WorldPacket { .. }) {
                self.login_trace_remaining -= 1;
            }
            if let Some(dir) = default_logs_dir() {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("login_trace.log"))
                {
                    use std::io::Write;
                    let _ = writeln!(f, "{msg:?}");
                }
            }
        }
        match msg {
            ServerMessage::Text(t) => {
                let line = rewrite_pounce_add_message(parse_chat_line(&t));
                for span in &line.spans {
                    if let furcadia_client::ChatSpan::Image { src, .. } = span {
                        let url = src.trim();
                        let lower = url.to_ascii_lowercase();
                        if (lower.starts_with("http://") || lower.starts_with("https://"))
                            && !self.chat_img_textures.contains_key(url)
                            && self.pending_chat_img_downloads.insert(url.to_owned())
                        {
                            let _ = self
                                .to_net
                                .send(UiToNet::DownloadChatImage { url: url.to_owned() });
                        }
                    }
                }
                let plain = line.to_plain_text();
                let (collected, total) = parse_cookie_counts(&plain);
                if collected.is_some() {
                    self.cookies_collected = collected;
                }
                if total.is_some() {
                    self.cookies_total = total;
                }
                let peer = if let Some(peer) = extract_whisper_peer(&plain) {
                    Some(peer)
                } else {
                    match_pending_whisper_echo(
                        &plain,
                        &self.active_character_name(),
                        &mut self.pending_whisper_echoes,
                    )
                };
                if let Some(peer) = peer {
                    let self_name = self.active_character_name();
                    let formatted = format_whisper_tab_line(&self_name, &peer, &plain, &line);
                    let idx = self.open_whisper_tab(&peer, false);
                    self.log_whisper_line(&peer, &plain);
                    let is_focused = !self.whispers[idx].detached
                        && self
                            .active_chat_tab
                            .as_deref()
                            .is_some_and(|a| a.eq_ignore_ascii_case(&peer));
                    if !is_focused && !self.whispers[idx].detached {
                        self.whispers[idx].unread += 1;
                    }
                    push_bounded_chat_line(&mut self.whispers[idx].lines, formatted);
                    if is_focused && self.chat_pinned_to_bottom {
                        self.chat_scroll_to_bottom = true;
                    }
                    let is_incoming_whisper = !plain.starts_with("[ You whisper ")
                        && !plain.starts_with("You whisper ");
                    if is_incoming_whisper {
                        if self.notify_whisper {
                            self.play_sound("whisper.wav");
                            self.send_desktop_notification(&format!("Whisper from {peer}"), &plain);
                        }
                        if self.flash_taskbar_on_message {
                            self.request_taskbar_flash();
                        }
                    }
                    self.last_received_whisper_from = peer;
                } else {
                    if should_suppress_silent_tab_glook_text(
                        &mut self.pending_silent_tab_glooks,
                        &plain,
                        Instant::now(),
                    ) {
                        return;
                    }
                    let me = self.active_character_name();
                    let mut flashed = false;
                    if self.notify_mention {
                        if let Some(matched) =
                            find_chat_mention(&plain, &me, &self.mention_nicknames)
                        {
                            let matched_owned = matched.to_owned();
                            self.play_sound("talk.wav");
                            self.send_desktop_notification(
                                &format!("Mentioned ({matched_owned})"),
                                &plain,
                            );
                            if self.flash_taskbar_on_message {
                                self.request_taskbar_flash();
                                flashed = true;
                            }
                        }
                    }
                    if !flashed
                        && self.flash_taskbar_on_main_chat
                        && !is_self_or_system_chat_line(&plain, &me)
                    {
                        self.request_taskbar_flash();
                    }
                    if self.active_chat_tab.is_some() {
                        self.main_chat_unread += 1;
                    }
                    self.log_chat_line(&plain);
                    push_bounded_chat_line(&mut self.chat_log, line);
                    if self.active_chat_tab.is_none() && self.chat_pinned_to_bottom {
                        self.chat_scroll_to_bottom = true;
                    }
                }
            }
            ServerMessage::LoginAck => {
                self.connected_at.get_or_insert_with(Instant::now);
                self.connecting_stage = "Logged in! Loading dream...".to_owned();
                self.push_system_line("success", "Login acknowledged. Entering world...");
                self.pending_vascodagama = true;
                self.login_ack_at = Some(Instant::now());
                self.login_trace_remaining = 60;
                // Furcadia's `info` response includes the character's cookie counts.
                let _ = self
                    .to_net
                    .send(UiToNet::Send(ClientCommand::Raw("info".to_owned())));
                // Begin loading Pounce data as soon as the game session accepts
                // the character. The Pounce panel itself is only rendered in-game.
                let _ = self
                    .to_net
                    .send(UiToNet::Send(ClientCommand::Raw("pounceInit".to_owned())));
                if let Some(dir) = default_logs_dir() {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(dir.join("login_trace.log"), "");
                }
            }
            ServerMessage::CharacterIdentity { uid, name } => {
                self.world.self_uid = uid.parse().ok();
                self.world.self_name = Some(name.clone());
                self.refresh_player_badge_colors();
                self.sync_selected_character_colors();
                self.push_system_line(
                    "success",
                    &format!("Logged in as {} (UID {uid})", name.replace('|', " ")),
                );
                let self_wire = wire_player_name(&name);
                if !self_wire.is_empty() {
                    self.silent_self_glook = true;
                    let _ = self
                        .to_net
                        .send(UiToNet::Send(ClientCommand::Glook(self_wire)));
                }
            }
            ServerMessage::MapName(_name) => {
                // `]m<name>` is the server's legacy area label (e.g. "arco 99"), not a dream/map
                // filename; `;` (UseMap) and `]q`/`]r` carry the actual map/dream package to load.
            }
            ServerMessage::WorldPacket { opcode, data } => {
                let prev_self_pose = self
                    .world
                    .self_uid
                    .and_then(|uid| self.world.avatars.get(&uid))
                    .map(|av| av.pose);
                self.world.apply_world_packet(opcode, &data);
                if let Some(new_pose) = self
                    .world
                    .self_uid
                    .and_then(|uid| self.world.avatars.get(&uid))
                    .map(|av| av.pose)
                {
                    if prev_self_pose != Some(new_pose) {
                        self.posture = posture_from_wire_pose(new_pose);
                    }
                }
                self.sync_selected_character_colors();
                self.refresh_player_badge_colors();
                let now = Instant::now();
                let mut visible_names: Vec<String> = Vec::new();
                for av in self.world.avatars.values() {
                    if Some(av.uid) != self.world.self_uid && self.pounce.contains(&av.name) {
                        visible_names.push(av.name.clone());
                    }
                }
                for name in visible_names {
                    let already_online = self
                        .pounce
                        .entries()
                        .any(|e| e.name.eq_ignore_ascii_case(&name) && e.status == OnlineStatus::Online);
                    if !already_online {
                        if let Some(PounceEvent::CameOnline(n)) =
                            self.pounce.handle_online_status(&name, true, now)
                        {
                            self.push_system_line("success", &format!("[Pounce] {n} came ONLINE!"));
                        }
                    }
                }
            }
            ServerMessage::RegionDefaults(data) => {
                self.world.apply_region_defaults(&data);
            }
            ServerMessage::OnlineStatus { online, name } => {
                match self
                    .pounce
                    .handle_online_status(&name, online, Instant::now())
                {
                    Some(PounceEvent::CameOnline(n)) => {
                        self.push_system_line("success", &format!("[Pounce] {n} came ONLINE!"));
                        if self.notify_pounce {
                            self.play_sound("online.wav");
                            self.send_desktop_notification("Pounce", &format!("{n} came online!"));
                        }
                    }
                    Some(PounceEvent::WentOffline(n)) => {
                        self.push_system_line("warning", &format!("[Pounce] {n} went OFFLINE."));
                        if self.notify_pounce {
                            self.play_sound("offline.wav");
                        }
                    }
                    Some(PounceEvent::Checked { .. }) => {}
                    None => {
                        let state = if online { "ONLINE" } else { "offline" };
                        self.push_system_line("emit", &format!("{name} is {state}."));
                    }
                }
            }
            ServerMessage::Extended { code, payload } => {
                if matches!(code, Some('q' | 'r')) {
                    self.handle_extended_dream(&payload);
                } else if code == Some('J') {
                    self.handle_use_map(&payload);
                } else if code == Some('s') {
                    self.world.apply_show_dream_name(&payload);
                } else if code == Some('t') {
                    self.world.apply_remove_dream_name(&payload);
                } else if code == Some('&') {
                    self.handle_portrait_directive(&payload);
                } else if code == Some('M') {
                    self.apply_dynamic_avatar_details(payload.as_bytes());
                } else if code == Some('o') {
                    let slug = payload.trim().trim_start_matches("furc://").trim_matches('/');
                    if !slug.is_empty() {
                        self.current_dream_url = Some(format!("furc://{slug}"));
                    }
                }
                if self.show_debug_packets {
                    let c = code.unwrap_or('?');
                    self.push_system_line("emit", &format!("]{c} {payload}"));
                }
            }
            ServerMessage::ExtendedBytes { code, data } => {
                match code {
                    b's' => self.world.apply_show_dream_name_bytes(&data),
                    b't' => self.world.apply_remove_dream_name_bytes(&data),
                    b'q' | b'r' => {
                        let payload = furcadia_client::encoding::decode_win1252(&data);
                        self.handle_extended_dream(&payload);
                    }
                    b'J' => {
                        let payload = furcadia_client::encoding::decode_win1252(&data);
                        self.handle_use_map(&payload);
                    }
                    b'_' => self.world.apply_avatar_scale(&data),
                    b'o' => {
                        let payload = furcadia_client::encoding::decode_win1252(&data);
                        let slug = payload.trim().trim_start_matches("furc://").trim_matches('/');
                        if !slug.is_empty() {
                            self.current_dream_url = Some(format!("furc://{slug}"));
                        }
                    }
                    b'H' => self.world.apply_avatar_offset(&data),
                    b'O' => self.world.apply_avatar_gloaming(&data),
                    b'}' => {
                        self.world.apply_self_color_bytes(&data);
                        self.sync_selected_character_colors();
                        if let Some(uid) = self.world.self_uid {
                            if let Some(av) = self.world.avatars.get(&uid) {
                                let key = shortname_key(&av.name);
                                if !key.is_empty() {
                                    self.player_badge_colors
                                        .insert(key, name_badge_chat_color(Some(uid) == self.world.self_uid));
                                }
                            }
                        }
                    }
                    b'M' => self.apply_dynamic_avatar_details(&data),
                    b'f' => {
                        if let Some(info) = WorldState::parse_looked_player_bytes(&data) {
                            let short_name = wire_player_name(&info.name);
                            let short_key = shortname_key(&short_name);
                            let is_self_name = self
                                .world
                                .self_name
                                .as_deref()
                                .is_some_and(|n| shortname_key(n) == short_key)
                                || shortname_key(&self.active_character_name()) == short_key;
                            if self.silent_self_glook && is_self_name {
                                self.silent_self_glook = false;
                                self.recent_portrait_target =
                                    Some((short_name, Instant::now()));
                            } else {
                                self.silent_self_glook = false;
                                self.recent_portrait_target =
                                    Some((short_name.clone(), Instant::now()));
                                if self
                                    .pending_glook_target
                                    .as_ref()
                                    .is_some_and(|(target, _)| {
                                        shortname_key(target) != short_key
                                    })
                                {
                                    self.pending_glook_target = None;
                                }
                                let portrait_id = self
                                    .player_portrait_ids
                                    .get(&short_key)
                                    .copied();
                                let card = LookedPlayerCard {
                                    info,
                                    short_name,
                                    portrait_id,
                                    portrait_tex: None,
                                    badge_tex: None,
                                    resolved_for: None,
                                };
                                apply_looked_card_to_tabs(
                                    card,
                                    self.active_chat_tab.as_deref(),
                                    &mut self.looked_player,
                                    &mut self.main_looked_player,
                                    &mut self.whispers,
                                    &mut self.known_player_cards,
                                    &mut self.pending_silent_tab_glooks,
                                    Instant::now(),
                                );
                            }
                        }
                    }
                    b'&' => {
                        let raw = furcadia_client::encoding::decode_win1252(&data);
                        self.handle_portrait_directive(&raw);
                    }
                    b'?' => {
                        for ev in self.pounce.apply_server_pounce(&data, Instant::now()) {
                            match ev {
                                PounceEvent::CameOnline(n) => {
                                    self.push_system_line(
                                        "success",
                                        &format!("[Pounce] {n} is now online!"),
                                    );
                                    if self.notify_pounce {
                                        self.play_sound("online.wav");
                                        self.send_desktop_notification(
                                            "Pounce",
                                            &format!("{n} is now online!"),
                                        );
                                    }
                                }
                                PounceEvent::WentOffline(n) => {
                                    self.push_system_line(
                                        "warning",
                                        &format!("[Pounce] {n} went offline."),
                                    );
                                    if self.notify_pounce {
                                        self.play_sound("offline.wav");
                                    }
                                }
                                PounceEvent::Checked { .. } => {}
                            }
                        }
                    }
                    _ => {}
                }
                if self.show_debug_packets {
                    let c = code as char;
                    self.push_system_line("emit", &format!("]{c} ({} bytes)", data.len()));
                }
            }
            ServerMessage::UseMap(name) => {
                self.handle_use_map(&name);
            }
            ServerMessage::Dragonroar { .. } => {}
            ServerMessage::Raw(raw) => {
                if self.show_debug_packets {
                    self.push_system_line("emit", &format!("[raw] {raw}"));
                }
            }
        }
    }

    fn spellcheck_dict_with_names(&self) -> std::collections::HashSet<String> {
        let mut dict = self.custom_dict_words.clone();
        let mut add_name = |name: &str| {
            for part in name.split(|c: char| !c.is_ascii_alphabetic() && c != '\'') {
                let w = part.trim_matches('\'').to_ascii_lowercase();
                if w.len() >= 2 {
                    dict.insert(w);
                }
            }
        };
        for av in self.world.avatars.values() {
            add_name(&av.name);
        }
        for entry in self.pounce.entries() {
            add_name(&entry.name);
        }
        for w in &self.whispers {
            add_name(&w.peer);
        }
        if let Some(lp) = &self.looked_player {
            add_name(&lp.info.name);
        }
        if let Some(info) = &self.account_info {
            for ch in &info.characters {
                add_name(&ch.name);
            }
        }
        dict
    }

    fn add_word_to_dictionary(&mut self, word: &str) {
        let trimmed = word.trim();
        if trimmed.is_empty() {
            return;
        }
        let lower = trimmed.to_ascii_lowercase();
        if self.custom_dict_words.insert(lower.clone()) {
            if let Some(path) = custom_dictionary_path() {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    use std::io::Write;
                    let _ = writeln!(f, "{trimmed}");
                }
            }
        }
        self.spell_misspelled
            .retain(|m| !m.word.eq_ignore_ascii_case(trimmed));
    }

    fn send_chat_input(&mut self) {
        let mut raw = self.chat_input.trim().to_owned();
        if raw.is_empty() || !self.is_connected {
            return;
        }
        if self.autocorrect
            && !raw.starts_with(['/', '`'])
            && !Self::is_bare_furcadia_command(&raw)
        {
            raw = autocorrect_common_words(&raw);
        }
        self.last_sent_msg = raw.clone();
        self.chat_input.clear();
        self.save_active_chat_draft();
        self.spell_misspelled.clear();
        self.spell_dirty_at = None;
        self.chat_pinned_to_bottom = true;
        self.chat_scroll_to_bottom = true;

        let (cmd, open_whisper_tab_disp) = classify_chat_input(
            &raw,
            self.active_chat_tab.as_deref(),
            self.whisper_emote_in_whisper,
        );
        if let Some(disp) = open_whisper_tab_disp {
            self.last_whisper_target = disp.clone();
            self.open_whisper_tab(&disp, true);
        } else if matches!(cmd, ClientCommand::Whisper { .. }) {
            if let Some(peer) = self.active_chat_tab.clone() {
                self.last_whisper_target = peer;
            }
        }

        if let ClientCommand::Whisper { target, message } = &cmd {
            self.track_pending_whisper_echo(target, message);
        }
        let glook_target = match &cmd {
            ClientCommand::Glook(target) => Some(target.clone()),
            ClientCommand::Raw(raw) => {
                let mut parts = raw.trim().splitn(2, char::is_whitespace);
                parts
                    .next()
                    .filter(|verb| verb.eq_ignore_ascii_case("glook"))
                    .and_then(|_| parts.next())
                    .map(wire_player_name)
            }
            _ => None,
        };
        if let Some(target) = glook_target {
            let target_key = shortname_key(&target);
            self.pending_silent_tab_glooks
                .retain(|e| e.target_key != target_key);
            self.silent_self_glook = false;
            self.pending_glook_target = Some((target, Instant::now()));
        }

        let _ = self.to_net.send(UiToNet::Send(cmd));
    }

    fn is_bare_furcadia_command(raw: &str) -> bool {
        let lower = raw.to_ascii_lowercase();
        let word = lower.split_whitespace().next().unwrap_or("");
        is_hyphenated_speech_furcadia_command(raw)
            || matches!(
                word,
                "whispersoff"
                    | "whisperson"
                    | "unafk"
                    | "desctags"
                    | "dreamurl"
                    | "liedown"
                    | "listgroups"
                    | "listf"
                    | "banish-off"
                    | "banish-list"
                    | "glook"
                    | "chdesc"
                    | "tempbanish"
                    | "<"
                    | ">"
            )
            || matches!(lower.as_str(), "ignore off" | "list ignores" | "m 1" | "m 3" | "m 7" | "m 9")
    }
}

/// Hyphenated Furcadia server commands handled via the speech (`"`) wire prefix.
/// Safe to recognize bare even inside whisper tabs (unlike plain English `"give"`).
fn is_hyphenated_speech_furcadia_command(raw: &str) -> bool {
    let first = raw
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "make-cookie"
            | "give-cookie"
            | "make-secret"
            | "secret-cookie"
            | "eat-cookie"
            | "munch-cookie"
            | "buy-cookie"
            | "cookies-reject"
            | "cookies-accept"
            | "cookies-off"
            | "cookies-on"
    ) || first.starts_with("trade-")
        || first.starts_with("gift-")
        || first.starts_with("give-")
        || first.starts_with("discard-")
}

/// All Furcadia server commands that require the speech (`"`) wire prefix when invoked
/// via `/cmd`, `` `cmd ``, `command://`, or UI menus (includes `"give"`).
fn is_speech_prefixed_furcadia_command(raw: &str) -> bool {
    if is_hyphenated_speech_furcadia_command(raw) {
        return true;
    }
    raw.split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("give"))
}

/// Formats a command passed to `send_raw_cmd` for the Furcadia wire protocol,
/// prepending `"` for speech-hook commands (`trade-*`, `gift-*`, `give-*`, cookies, etc.).
fn format_raw_cmd_wire(cmd: &str) -> String {
    let trimmed = cmd.trim();
    if is_speech_prefixed_furcadia_command(trimmed) {
        format!("\"{trimmed}")
    } else {
        trimmed.to_owned()
    }
}

/// Maps a Furcadia avatar wire pose (`0..=6`) to the UI posture index (`0=Stand`, `1=Sit`, `2=Lie Down`).
fn posture_from_wire_pose(pose: u8) -> u8 {
    match pose {
        3 => 1,
        6 => 2,
        _ => 0,
    }
}

/// Returns the explicit Furcadia wire command for a UI posture index (`0=Stand`, `1=Sit`, `2=Lie Down`).
/// Uses `liedown` rather than `lie` (which is the server's toggle command) so lay down is never skipped.
fn posture_wire_command(posture: u8) -> &'static str {
    match posture {
        1 => "sit",
        2 => "liedown",
        _ => "stand",
    }
}

/// Parses comma, semicolon, or newline-separated custom mention nicknames.
fn parse_mention_nicknames(raw: &str) -> Vec<&str> {
    raw.split([',', ';', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Case-insensitive word-boundary match so short nicknames (e.g. `"Aur"`) match
/// `"hugs Aur!"` or `"Aur's"` without false-triggering on `"taur"` or `"aura"`.
fn contains_word_ci(haystack: &str, needle: &str) -> bool {
    let needle_trimmed = needle.trim();
    if needle_trimmed.is_empty() {
        return false;
    }
    let h = haystack.to_ascii_lowercase();
    let n = needle_trimmed.to_ascii_lowercase();
    let mut start = 0;
    while let Some(rel) = h[start..].find(&n) {
        let idx = start + rel;
        let end = idx + n.len();
        let before_ok = h[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = h[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
        while start < h.len() && !h.is_char_boundary(start) {
            start += 1;
        }
    }
    false
}

/// Returns `true` if `plain` is spoken/emoted by `char_name` itself or is a system/status line.
fn is_self_or_system_chat_line(plain: &str, char_name: &str) -> bool {
    let trimmed = plain.trim_start();
    if trimmed.is_empty()
        || trimmed.starts_with("[*]")
        || trimmed.starts_with("[Pounce]")
        || trimmed.starts_with("[ You ")
        || trimmed.starts_with("You ")
        || trimmed.starts_with("Current players:")
    {
        return true;
    }
    let me = char_name.trim();
    if !me.is_empty() && me != "Unknown" {
        let lower_line = trimmed.to_ascii_lowercase();
        let lower_me = me.to_ascii_lowercase();
        if let Some(rest) = lower_line.strip_prefix(&lower_me) {
            if rest.is_empty()
                || rest.starts_with(':')
                || rest.starts_with(' ')
                || rest.starts_with('\'')
            {
                return true;
            }
        }
    }
    false
}

/// Checks whether a Main Chat line mentions `char_name` or any nickname in `nicknames_raw`,
/// returning the first matched term. Skips self-spoken/emoted lines and system notices.
fn find_chat_mention<'a>(plain: &str, char_name: &'a str, nicknames_raw: &'a str) -> Option<&'a str> {
    if is_self_or_system_chat_line(plain, char_name) {
        return None;
    }
    let me = char_name.trim();
    if !me.is_empty() && me != "Unknown" && contains_word_ci(plain, me) {
        return Some(me);
    }
    parse_mention_nicknames(nicknames_raw).into_iter().find(|&nick| contains_word_ci(plain, nick)).map(|v| v as _)
}

/// Builds a single-line Furcadia trade command (`trade-add <item>`, `trade-remove <item>`,
/// `trade-reason <text>`), sanitizing embedded newlines and returning `None` if empty.
fn build_trade_command(verb: &str, arg: &str) -> Option<String> {
    let cleaned = arg.replace(['\r', '\n'], " ");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(format!("{verb} {trimmed}"))
    }
}

/// Common Furcadia Secure Trade items grouped for the `Trade -> Add Common Item` submenu.
const COMMON_TRADE_ITEM_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Portraits",
        &[
            ("Portrait Space", "portrait"),
            ("Animated Portrait", "animated-portrait"),
        ],
    ),
    (
        "Wings",
        &[
            ("Classic Wings", "classicwings"),
            ("Butterfly Wings", "butterflywings"),
            ("Bat Wings", "batwings"),
            ("Prime Wings", "primewings"),
            ("Tri-Wings", "triwings"),
        ],
    ),
    (
        "Gifts & Desctags",
        &[
            ("Kitterdust", "kitterdust"),
            ("Bouquet", "bouquet"),
            ("Red Rose", "red-rose"),
            ("Black Rose", "black-rose"),
            ("White Rose", "white-rose"),
            ("Purple Rose", "purple-rose"),
            ("Yellow Rose", "yellow-rose"),
            ("Flaming Sword", "flaming-sword"),
            ("Sword", "sword"),
            ("Magic Wand", "magic-wand"),
            ("Snug", "snug"),
            ("Friendship Bracelet", "friendship-bracelet"),
            ("Candy Cane", "candy-cane"),
            ("Valentine", "valentine"),
            ("Fancy Valentine", "valentine-fancy"),
        ],
    ),
];

/// Classifies trimmed user chat input into `(ClientCommand, Option<open_whisper_tab_display_name>)`.
fn classify_chat_input(
    raw: &str,
    active_whisper_peer: Option<&str>,
    whisper_emote_in_whisper: bool,
) -> (ClientCommand, Option<String>) {
    let mut open_whisper_tab_disp = None;
    let cmd = if let Some(raw_cmd) = raw.strip_prefix('`') {
        let trimmed = raw_cmd.trim();
        if is_speech_prefixed_furcadia_command(trimmed) {
            ClientCommand::Say(trimmed.to_owned())
        } else {
            ClientCommand::Raw(raw_cmd.to_owned())
        }
    } else if let Some(after_slash) = raw.strip_prefix('/') {
        if after_slash.starts_with("%%") || after_slash.starts_with('%') {
            // Offline message (/%%name msg) or exact-name whisper (/%name msg).
            ClientCommand::Raw(raw.to_owned())
        } else if let Some(rest) = after_slash.strip_prefix('"') {
            // Quoted whisper: /"furre name" message
            if let Some((target, msg)) = rest.split_once('"') {
                let msg = msg.trim_start();
                open_whisper_tab_disp = Some(target.replace('|', " "));
                ClientCommand::Whisper {
                    target: wire_player_name(target),
                    message: msg.to_owned(),
                }
            } else {
                ClientCommand::Raw(raw.to_owned())
            }
        } else if let Some(rest) = after_slash
            .strip_prefix("wh ")
            .or_else(|| after_slash.strip_prefix("w "))
        {
            if let Some((target, msg)) = rest.split_once(' ') {
                open_whisper_tab_disp = Some(target.replace('|', " "));
                ClientCommand::Whisper {
                    target: wire_player_name(target),
                    message: msg.to_owned(),
                }
            } else {
                ClientCommand::Raw(raw.to_owned())
            }
        } else if let Some(name) = after_slash.strip_prefix("onln ") {
            ClientCommand::OnlineCheck(wire_player_name(name))
        } else if let Some(name) = after_slash.strip_prefix("glook ") {
            ClientCommand::Glook(wire_player_name(name))
        } else if after_slash.eq_ignore_ascii_case("who") {
            ClientCommand::Who
        } else if let Some(line) = after_slash.strip_prefix("raw ") {
            ClientCommand::Raw(line.to_owned())
        } else if is_speech_prefixed_furcadia_command(after_slash) {
            ClientCommand::Say(after_slash.trim().to_owned())
        } else if let Some((target, msg)) = after_slash.split_once(' ') {
            let first_lower = target.to_ascii_lowercase();
            if matches!(
                first_lower.as_str(),
                "help" | "look" | "ignore" | "banish" | "tempbanish" | "summon" | "join"
                    | "lead" | "follow" | "desc" | "chdesc" | "roll" | "m" | "jg" | "chat"
                    | "afk" | "fdl"
            ) {
                ClientCommand::Raw(after_slash.to_owned())
            } else {
                // Furcadia `/furrename message` shorthand whisper
                open_whisper_tab_disp = Some(target.replace('|', " "));
                ClientCommand::Whisper {
                    target: wire_player_name(target),
                    message: msg.to_owned(),
                }
            }
        } else {
            // Single-word `/command` such as `/info`, `/time`, `/sit`, `/dreamurl`, etc.
            ClientCommand::Raw(after_slash.to_owned())
        }
    } else if raw.starts_with('=') {
        // Channel broadcasts (=talz, =news, =fun, etc.)
        ClientCommand::Raw(raw.to_owned())
    } else if raw.starts_with(':') && is_whisper_colon_emote(raw) {
        if let Some(peer) = active_whisper_peer.filter(|_| whisper_emote_in_whisper) {
            ClientCommand::Whisper {
                target: wire_player_name(peer),
                message: raw.to_owned(),
            }
        } else {
            ClientCommand::Raw(raw.to_owned())
        }
    } else if FurcadiaApp::is_bare_furcadia_command(raw) {
        if is_hyphenated_speech_furcadia_command(raw) {
            ClientCommand::Say(raw.to_owned())
        } else {
            ClientCommand::Raw(raw.to_owned())
        }
    } else if let Some(peer) = active_whisper_peer {
        ClientCommand::Whisper {
            target: wire_player_name(peer),
            message: raw.to_owned(),
        }
    } else {
        ClientCommand::Say(raw.to_owned())
    };
    (cmd, open_whisper_tab_disp)
}

fn detect_window_backend(_cc: &eframe::CreationContext<'_>) -> (bool, Option<u32>) {
    #[cfg(target_os = "linux")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = _cc.window_handle() {
            match handle.as_raw() {
                RawWindowHandle::Wayland(_) => return (true, None),
                RawWindowHandle::Xlib(h) => return (false, u32::try_from(h.window).ok()),
                RawWindowHandle::Xcb(h) => return (false, Some(h.window.get())),
                _ => return (false, None),
            }
        }
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
        (has_wayland, None)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (false, None)
    }
}

/// Performs an ICCCM §4.1.4 `XWithdrawWindow` (UnmapWindow + synthetic UnmapNotify to root).
///
/// `winit`'s X11 `set_visible(false)` only calls `xcb_unmap_window`, which is a no-op when
/// the window manager (e.g. KWin on KDE Plasma) has already unmapped/iconified the window
/// on minimize. Sending the synthetic `UnmapNotify` transitions the window from Iconic to
/// Withdrawn so it actually disappears from the taskbar/panel into the system tray.
#[cfg(target_os = "linux")]
fn withdraw_x11_window(window_id: u32) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        ConnectionExt, EventMask, UnmapNotifyEvent, UNMAP_NOTIFY_EVENT,
    };
    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return;
    };
    let Some(screen) = conn.setup().roots.get(screen_num) else {
        return;
    };
    let root = screen.root;
    let _ = conn.unmap_window(window_id);
    let event = UnmapNotifyEvent {
        response_type: UNMAP_NOTIFY_EVENT,
        sequence: 0,
        event: root,
        window: window_id,
        from_configure: false,
    };
    let _ = conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    );
    let _ = conn.flush();
}

impl FurcadiaApp {
    fn hide_to_tray(&mut self, ctx: &egui::Context) {
        if self.tray.is_active() && !self.is_wayland {
            self.hidden_in_tray = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            #[cfg(target_os = "linux")]
            if let Some(win_id) = self.x11_window_id {
                withdraw_x11_window(win_id);
            }
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
    }

    fn show_from_tray(&mut self, ctx: &egui::Context) {
        self.hidden_in_tray = false;
        self.tray.set_attention(false);
        self.was_minimized = true;
        self.tray_restore_until = Some(Instant::now() + Duration::from_millis(500));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    fn poll_tray_events(&mut self, ctx: &egui::Context) {
        while let Some(action) = self.tray.try_recv() {
            match action {
                TrayAction::Toggle => {
                    let is_minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
                    if self.hidden_in_tray || is_minimized {
                        self.show_from_tray(ctx);
                    } else {
                        self.hide_to_tray(ctx);
                    }
                }
                TrayAction::Show => self.show_from_tray(ctx),
                TrayAction::Hide => self.hide_to_tray(ctx),
                TrayAction::Quit => {
                    if self.has_multiple_character_tabs() {
                        self.show_from_tray(ctx);
                        self.close_confirm_open = true;
                    } else {
                        self.quit_all_sessions(ctx);
                    }
                }
            }
        }
    }
    fn track_window_geometry(&mut self, ctx: &egui::Context, is_minimized: bool) {
        let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
        let mut changed = false;
        if maximized != self.window_maximized {
            self.window_maximized = maximized;
            changed = true;
        }
        if !maximized && !is_minimized && !self.hidden_in_tray {
            let size = ctx
                .input(|i| i.viewport().inner_rect.map(|r| r.size()))
                .unwrap_or_else(|| ctx.screen_rect().size());
            if size.x.is_finite() && size.y.is_finite() && size.x >= 640.0 && size.y >= 420.0 {
                let w = size.x.round().clamp(640.0, 7680.0) as u16;
                let h = size.y.round().clamp(420.0, 4320.0) as u16;
                if w != self.window_width || h != self.window_height {
                    self.window_width = w;
                    self.window_height = h;
                    changed = true;
                }
            }
        }
        if changed {
            self.window_geometry_dirty_at = Some(Instant::now());
        }
        let flush_now = self
            .window_geometry_dirty_at
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(400));
        if flush_now {
            self.window_geometry_dirty_at = None;
            let _ = self.current_config().save_local_default();
        }
    }

    fn flush_window_geometry(&mut self) {
        if self.window_geometry_dirty_at.take().is_some() {
            let _ = self.current_config().save_local_default();
        }
    }
}

impl eframe::App for FurcadiaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_font_settings(ctx);
        self.poll_tray_events(ctx);
        self.window_focused = ctx.input(|i| i.focused);
        let close_req = ctx.input(|i| i.viewport().close_requested());
        if close_req {
            self.flush_window_geometry();
        }
        if close_req && !self.allow_quit {
            if self.close_to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide_to_tray(ctx);
            } else if self.has_multiple_character_tabs() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.close_confirm_open = true;
            }
        }
        let is_minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        self.track_window_geometry(ctx, is_minimized);
        let restoring = self
            .tray_restore_until
            .is_some_and(|until| Instant::now() < until);
        if !restoring {
            self.tray_restore_until = None;
        }
        if self.minimize_to_tray
            && !restoring
            && is_minimized
            && !self.was_minimized
            && !self.hidden_in_tray
            && self.tray.is_active()
            && !self.is_wayland
        {
            self.hide_to_tray(ctx);
        }
        self.was_minimized = is_minimized;
        // winit's X11 backend does not emit a WindowEvent on `_NET_WM_STATE` PropertyNotify,
        // so poll lightly while unfocused and visible to catch minimize-button / taskbar iconify.
        if self.minimize_to_tray
            && !self.hidden_in_tray
            && self.tray.is_active()
            && !self.is_wayland
            && !self.window_focused
        {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        self.handle_net_events(ctx);
        self.poll_bg_sessions();
        self.flush_taskbar_flash(ctx);

        if self.is_connected {
            if self.pending_vascodagama {
                if self
                    .login_ack_at
                    .is_some_and(|t| t.elapsed() >= Duration::from_millis(500))
                {
                    self.send_pending_vascodagama();
                } else {
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
            }
            if let Some(target) = self.pounce.next_due_check(Instant::now()) {
                let _ = self
                    .to_net
                    .send(UiToNet::Send(ClientCommand::OnlineCheck(wire_player_name(
                        &target,
                    ))));
            }
            if self.pounce.has_unknown() {
                ctx.request_repaint_after(Duration::from_millis(45));
            }
        } else if self.reconnecting {
            ctx.request_repaint_after(Duration::from_millis(50));
        }

        if self.screen == AppScreen::Connecting {
            let ready_for_world = self.is_connected
                && (self.world.map.is_some()
                    || self
                        .connected_at
                        .is_some_and(|t| t.elapsed() >= Duration::from_secs(4)));
            if ready_for_world {
                self.screen = AppScreen::InGame;
            } else if self.connecting_error.is_none() {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }

        if self.bear_logo.is_none() {
            self.bear_logo = Some(logo::load_logo(ctx));
        }

        match self.screen {
            AppScreen::Login => self.render_login_screen(ctx),
            AppScreen::CharacterSelect => self.render_character_select_screen(ctx),
            AppScreen::Connecting => self.render_connecting_screen(ctx),
            AppScreen::InGame => self.render_in_game_screen(ctx),
        }


        if self.log_viewer_open {
            self.render_log_viewer_window(ctx);
        }
        if self.help_open {
            self.render_help_window(ctx);
        }
        if self.about_open {
            self.render_about_window(ctx);
        }
        self.poll_update_check();
        self.poll_update_install(ctx);
        if self.first_setup_open {
            self.render_first_setup_window(ctx);
        }
        if self.update_popup_open {
            self.render_update_window(ctx);
        }
        if self.close_confirm_open {
            self.render_close_confirm_window(ctx);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_music();
        self.save_config();
    }
}

impl FurcadiaApp {
    fn trigger_update_check(&mut self, ctx: &egui::Context) {
        if self.update_checking {
            return;
        }
        self.update_checking = true;
        self.update_status = Some("Checking GitHub for updates...".to_owned());
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        let ctx_clone = ctx.clone();
        let current_ver = env!("CARGO_PKG_VERSION").to_owned();
        thread::spawn(move || {
            let out = furcadia_client::sys::command("curl")
                .args([
                    "-fsSL",
                    "-H",
                    "Accept: application/vnd.github+json",
                    "-H",
                    "User-Agent: furcadia-native",
                    "--max-time",
                    "10",
                    "https://api.github.com/repos/morganlowe/native-furcadia/releases/latest",
                ])
                .output();
            let msg = match out {
                Ok(o) if o.status.success() => {
                    let body = String::from_utf8_lossy(&o.stdout);
                    if let Some(info) = parse_github_release_json(&body) {
                        if is_newer_version(&current_ver, &info.version) {
                            UpdateCheckMsg::UpdateAvailable(info)
                        } else {
                            UpdateCheckMsg::UpToDate(info.version)
                        }
                    } else {
                        UpdateCheckMsg::Error("Could not parse GitHub release metadata.".to_owned())
                    }
                }
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr).trim().to_owned();
                    UpdateCheckMsg::Error(if err.is_empty() {
                        format!("HTTP/curl exited {}", o.status)
                    } else {
                        err
                    })
                }
                Err(e) => UpdateCheckMsg::Error(format!("Failed to run curl: {e}")),
            };
            let _ = tx.send(msg);
            ctx_clone.request_repaint();
        });
    }

    fn poll_update_check(&mut self) {
        let msg = match &self.update_rx {
            Some(rx) => match rx.try_recv() {
                Ok(m) => Some(m),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.update_checking = false;
                    self.update_rx = None;
                    None
                }
            },
            None => None,
        };
        if let Some(m) = msg {
            self.update_checking = false;
            self.update_rx = None;
            match m {
                UpdateCheckMsg::UpToDate(_latest) => {
                    self.update_available = None;
                    self.update_status = Some(format!(
                        "Up to date (v{}).",
                        env!("CARGO_PKG_VERSION")
                    ));
                }
                UpdateCheckMsg::UpdateAvailable(info) => {
                    self.update_status = Some(format!("Update available: {}", info.tag));
                    self.update_available = Some(info);
                    self.update_popup_open = true;
                }
                UpdateCheckMsg::Error(err) => {
                    self.update_status = Some(format!("Update check failed: {err}"));
                }
            }
        }
    }

    fn trigger_update_install(&mut self, ctx: &egui::Context) {
        if matches!(
            self.update_install_state,
            UpdateInstallState::Downloading | UpdateInstallState::Installing
        ) {
            return;
        }
        let Some(asset_url) = self
            .update_available
            .as_ref()
            .and_then(|info| info.asset_url.clone())
        else {
            self.update_install_state = UpdateInstallState::Failed(
                "No matching installer asset found for this platform.".to_owned(),
            );
            return;
        };

        self.update_install_state = UpdateInstallState::Downloading;
        let (tx, rx) = mpsc::channel();
        self.update_install_rx = Some(rx);
        let ctx_clone = ctx.clone();

        thread::spawn(move || {
            let filename = update_download_filename(&asset_url);
            let dest_path = std::env::temp_dir().join(&filename);
            let dest_str = dest_path.to_string_lossy().into_owned();

            let dl = furcadia_client::sys::command("curl")
                .args(["-fsSL", "--max-time", "180", "-o", &dest_str, &asset_url])
                .output();
            match dl {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    let _ = std::fs::remove_file(&dest_path);
                    let err = String::from_utf8_lossy(&o.stderr).trim().to_owned();
                    let _ = tx.send(UpdateInstallMsg::Failed(if err.is_empty() {
                        format!("Download failed (curl exited {}).", o.status)
                    } else {
                        format!("Download failed: {err}")
                    }));
                    ctx_clone.request_repaint();
                    return;
                }
                Err(e) => {
                    let _ = tx.send(UpdateInstallMsg::Failed(format!(
                        "Failed to run curl: {e}"
                    )));
                    ctx_clone.request_repaint();
                    return;
                }
            }

            #[cfg(target_os = "windows")]
            {
                let _ = tx.send(UpdateInstallMsg::LaunchInstallerAndExit(dest_path));
                ctx_clone.request_repaint();
                return;
            }

            #[cfg(target_os = "macos")]
            {
                let _ = tx.send(UpdateInstallMsg::OpenDmg(dest_path));
                ctx_clone.request_repaint();
                return;
            }

            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            {
                let _ = tx.send(UpdateInstallMsg::Installing);
                ctx_clone.request_repaint();

                let is_arch = filename.ends_with(".pkg.tar.zst");
                let is_rpm = filename.ends_with(".rpm");
                let is_deb = filename.ends_with(".deb");
                let install_res = if is_arch {
                    std::process::Command::new("pkexec")
                        .args(["pacman", "-U", "--noconfirm", &dest_str])
                        .output()
                } else if is_rpm {
                    if std::path::Path::new("/usr/bin/dnf").exists() {
                        std::process::Command::new("pkexec")
                            .args(["dnf", "install", "-y", &dest_str])
                            .output()
                    } else if std::path::Path::new("/usr/bin/zypper").exists() {
                        std::process::Command::new("pkexec")
                            .args([
                                "zypper",
                                "--non-interactive",
                                "install",
                                "--allow-unsigned-rpm",
                                &dest_str,
                            ])
                            .output()
                    } else {
                        std::process::Command::new("pkexec")
                            .args(["rpm", "-Uvh", &dest_str])
                            .output()
                    }
                } else if is_deb {
                    std::process::Command::new("pkexec")
                        .args(["apt-get", "install", "-y", &dest_str])
                        .output()
                } else {
                    let _ = std::fs::remove_file(&dest_path);
                    let _ = tx.send(UpdateInstallMsg::Failed(format!(
                        "Unsupported package format: {filename}"
                    )));
                    ctx_clone.request_repaint();
                    return;
                };

                let _ = std::fs::remove_file(&dest_path);
                match install_res {
                    Ok(o) if o.status.success() => {
                        let _ = tx.send(UpdateInstallMsg::Installed);
                    }
                    Ok(o) => {
                        let err = String::from_utf8_lossy(&o.stderr).trim().to_owned();
                        let msg = if o.status.code() == Some(126) || o.status.code() == Some(127) {
                            "Authorization cancelled or pkexec unavailable.".to_owned()
                        } else if err.is_empty() {
                            format!("Installer exited with {}.", o.status)
                        } else {
                            format!("Install failed: {err}")
                        };
                        let _ = tx.send(UpdateInstallMsg::Failed(msg));
                    }
                    Err(e) => {
                        let _ = tx.send(UpdateInstallMsg::Failed(format!(
                            "Failed to launch pkexec: {e}"
                        )));
                    }
                }
                ctx_clone.request_repaint();
            }
        });
    }

    fn poll_update_install(&mut self, #[allow(unused_variables)] ctx: &egui::Context) {
        let msg = match &self.update_install_rx {
            Some(rx) => match rx.try_recv() {
                Ok(m) => Some(m),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if matches!(
                        self.update_install_state,
                        UpdateInstallState::Downloading | UpdateInstallState::Installing
                    ) {
                        self.update_install_state = UpdateInstallState::Failed(
                            "Update worker stopped unexpectedly.".to_owned(),
                        );
                    }
                    self.update_install_rx = None;
                    None
                }
            },
            None => None,
        };
        if let Some(m) = msg {
            match m {
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                UpdateInstallMsg::Installing => {
                    self.update_install_state = UpdateInstallState::Installing;
                }
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                UpdateInstallMsg::Installed => {
                    self.update_install_rx = None;
                    self.update_install_state = UpdateInstallState::Installed;
                    self.update_status = Some("Update installed! Restart to apply.".to_owned());
                }
                #[cfg(target_os = "macos")]
                UpdateInstallMsg::OpenDmg(path) => {
                    self.update_install_rx = None;
                    match std::process::Command::new("open").arg(&path).spawn() {
                        Ok(_) => {
                            self.update_install_state = UpdateInstallState::Installed;
                            self.update_status = Some(
                                "Opened disk image — drag Furcadia Native Client to Applications and restart."
                                    .to_owned(),
                            );
                        }
                        Err(e) => {
                            self.update_install_state = UpdateInstallState::Failed(format!(
                                "Could not open disk image: {e}"
                            ));
                        }
                    }
                }
                #[cfg(target_os = "windows")]
                UpdateInstallMsg::LaunchInstallerAndExit(path) => {
                    self.update_install_rx = None;
                    match furcadia_client::sys::command(&path)
                        .arg("/CLOSEAPPLICATIONS")
                        .spawn()
                    {
                        Ok(_) => {
                            self.allow_quit = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(e) => {
                            self.update_install_state = UpdateInstallState::Failed(format!(
                                "Could not launch installer: {e}"
                            ));
                        }
                    }
                }
                UpdateInstallMsg::Failed(err) => {
                    self.update_install_rx = None;
                    self.update_install_state = UpdateInstallState::Failed(err);
                }
            }
        }
    }

    fn render_first_setup_window(&mut self, ctx: &egui::Context) {
        let accent = self.ui_accent();
        let mut open = self.first_setup_open;
        egui::Window::new("Welcome — First-Time Setup")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.heading(RichText::new("Furcadia Native Client").color(accent));
                ui.label(
                    RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                        .size(12.0)
                        .color(Color32::LIGHT_GRAY),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);

                ui.label(RichText::new("Automatic Updates").strong().color(accent));
                ui.checkbox(
                    &mut self.check_updates_on_startup,
                    "Automatically check GitHub for updates on startup",
                );
                ui.label(
                    RichText::new(
                        "You can change this anytime in Settings → Pounce, Tray & Updates.",
                    )
                    .size(11.0)
                    .color(Color32::GRAY),
                );

                ui.add_space(10.0);
                ui.label(RichText::new("Import Existing Furcadia Data").strong().color(accent));
                ui.label(
                    RichText::new(
                        "This imports available patches/assets, your Pounce watchlist, and chat and whisper logs. It also scans character INI files. The app checks Documents/Furcadia, My Documents/Furcadia, OneDrive/Documents/Furcadia, common Furcadia install folders, and Wine Documents/AppData folders.",
                    )
                    .size(12.0)
                    .color(Color32::LIGHT_GRAY),
                );
                if let Some(root) = &self.assets.root {
                    ui.colored_label(
                        Color32::from_rgb(130, 225, 145),
                        format!("Local assets found: {}", root.display()),
                    );
                }
                if self.import_source_detected {
                    ui.colored_label(
                        Color32::from_rgb(130, 225, 145),
                        format!("Existing Furcadia data found: {}", self.import_path_input),
                    );
                } else {
                    ui.label(
                        RichText::new(
                            "No Furcadia data found in the usual Documents or Wine folders. Choose your Windows Furcadia folder to import existing files.",
                        )
                        .size(12.0)
                        .color(Color32::LIGHT_GRAY),
                    );
                }
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.import_path_input)
                            .desired_width(270.0),
                    );
                    if ui.button("Browse...").clicked() {
                        self.browse_import_directory();
                    }
                });
                ui.horizontal(|ui| {
                    let import_path_exists = std::path::Path::new(self.import_path_input.trim()).is_dir();
                    if ui
                        .add_enabled(import_path_exists, egui::Button::new("Yes, import now"))
                        .clicked()
                    {
                        self.run_windows_import();
                    }
                    if ui.button("Not now").clicked() {
                        self.import_report = Some(
                            "You can import later from Settings → Import Windows Furcadia & Pounce Data.".to_owned(),
                        );
                    }
                });
                if let Some(report) = &self.import_report {
                    ui.colored_label(Color32::from_rgb(130, 225, 145), report);
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Finish Setup")
                                    .strong()
                                    .color(Color32::from_rgb(20, 14, 6)),
                            )
                            .fill(accent),
                        )
                        .clicked()
                    {
                        self.first_setup_done = true;
                        self.first_setup_open = false;
                        self.save_config();
                        if self.check_updates_on_startup {
                            self.trigger_update_check(ctx);
                        }
                    }
                });
            });
        if !open && self.first_setup_open {
            self.first_setup_done = true;
            self.first_setup_open = false;
            self.save_config();
        }
    }

    fn render_update_window(&mut self, ctx: &egui::Context) {
        let Some(info) = self.update_available.clone() else {
            self.update_popup_open = false;
            return;
        };
        let accent = self.ui_accent();
        let mut open = self.update_popup_open;
        egui::Window::new("Update Available")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.heading(RichText::new(format!("New version {} is available!", info.tag)).color(accent));
                ui.label(
                    RichText::new(format!(
                        "You are currently running v{}.",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .color(Color32::LIGHT_GRAY),
                );
                ui.add_space(10.0);
                match &self.update_install_state {
                    UpdateInstallState::Idle => {
                        ui.horizontal_wrapped(|ui| {
                            if info.asset_url.is_some()
                                && ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Download & Install")
                                                .strong()
                                                .color(Color32::from_rgb(20, 14, 6)),
                                        )
                                        .fill(accent),
                                    )
                                    .clicked()
                            {
                                self.trigger_update_install(ctx);
                            }
                            if ui.button("Open Release Page").clicked() {
                                ctx.open_url(egui::OpenUrl::new_tab(&info.html_url));
                            }
                            if ui.button("Later").clicked() {
                                self.update_popup_open = false;
                            }
                        });
                    }
                    UpdateInstallState::Downloading => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Downloading update package...");
                        });
                    }
                    UpdateInstallState::Installing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Installing update (waiting for authorization)...");
                        });
                    }
                    UpdateInstallState::Installed => {
                        ui.label(
                            RichText::new(
                                "Update installed! Restart the client to run the new version.",
                            )
                            .color(Color32::from_rgb(120, 220, 140)),
                        );
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("Restart Now")
                                            .strong()
                                            .color(Color32::from_rgb(20, 14, 6)),
                                    )
                                    .fill(accent),
                                )
                                .clicked()
                            {
                                let launched = std::env::current_exe()
                                    .map(|exe| {
                                        let s = exe.to_string_lossy();
                                        if let Some(stripped) = s.strip_suffix(" (deleted)") {
                                            std::path::PathBuf::from(stripped)
                                        } else {
                                            exe
                                        }
                                    })
                                    .and_then(|exe| {
                                        std::process::Command::new(&exe)
                                            .stdin(std::process::Stdio::null())
                                            .stdout(std::process::Stdio::null())
                                            .stderr(std::process::Stdio::null())
                                            .spawn()
                                            .or_else(|_| {
                                                std::process::Command::new("/usr/bin/furc-gui")
                                                    .stdin(std::process::Stdio::null())
                                                    .stdout(std::process::Stdio::null())
                                                    .stderr(std::process::Stdio::null())
                                                    .spawn()
                                            })
                                            .or_else(|_| {
                                                std::process::Command::new("furc-gui")
                                                    .stdin(std::process::Stdio::null())
                                                    .stdout(std::process::Stdio::null())
                                                    .stderr(std::process::Stdio::null())
                                                    .spawn()
                                            })
                                    });
                                match launched {
                                    Ok(mut child) => {
                                        // Give the new process a moment to initialize. If it
                                        // exits immediately, keep this window open and show why.
                                        thread::sleep(Duration::from_millis(600));
                                        match child.try_wait() {
                                            Ok(None) => {
                                                self.allow_quit = true;
                                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                            }
                                            Ok(Some(status)) => {
                                                self.update_install_state = UpdateInstallState::Failed(
                                                    format!("The updated client exited during startup ({status})."),
                                                );
                                            }
                                            Err(err) => {
                                                self.update_install_state = UpdateInstallState::Failed(
                                                    format!("Could not verify the updated client started: {err}"),
                                                );
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        self.update_install_state = UpdateInstallState::Failed(
                                            format!("Could not restart the client: {err}"),
                                        );
                                    }
                                }
                            }
                            if ui.button("Close").clicked() {
                                self.update_popup_open = false;
                            }
                        });
                    }
                    UpdateInstallState::Failed(err) => {
                        ui.label(
                            RichText::new(err)
                                .size(12.0)
                                .color(Color32::from_rgb(240, 110, 110)),
                        );
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            if info.asset_url.is_some()
                                && ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Retry Install")
                                                .strong()
                                                .color(Color32::from_rgb(20, 14, 6)),
                                        )
                                        .fill(accent),
                                    )
                                    .clicked()
                            {
                                self.trigger_update_install(ctx);
                            }
                            if ui.button("Open Release Page").clicked() {
                                ctx.open_url(egui::OpenUrl::new_tab(&info.html_url));
                            }
                            if ui.button("Later").clicked() {
                                self.update_popup_open = false;
                            }
                        });
                    }
                }
            });
        if !open {
            self.update_popup_open = false;
        }
    }

    fn render_close_confirm_window(&mut self, ctx: &egui::Context) {
        if !self.has_multiple_character_tabs() {
            self.close_confirm_open = false;
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_confirm_open = false;
            return;
        }

        let (current_char, all_names) = self.open_character_tab_names();
        let names_list = format_character_names(&all_names);
        let close_all_label = format_close_all_button_label(&all_names);
        let accent = self.ui_accent();
        let mut open = self.close_confirm_open;
        let mut cancel = false;
        let mut close_current_only = false;
        let mut close_new_tab = false;
        let mut close_all = false;

        egui::Window::new("Multiple Characters Open")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.heading(RichText::new("Multiple character tabs are open").color(accent));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "You have {} character tabs open: {names_list}.",
                        all_names.len()
                    ))
                    .color(Color32::LIGHT_GRAY),
                );
                ui.add_space(6.0);
                if let Some(current) = current_char.as_deref() {
                    ui.label(format!(
                        "Close current character {current} only, or close all ({names_list})?"
                    ));
                    ui.add_space(12.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(format!("Close {current} Only"))
                                        .strong()
                                        .color(Color32::from_rgb(20, 14, 6)),
                                )
                                .fill(accent),
                            )
                            .clicked()
                        {
                            close_current_only = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(&close_all_label)
                                        .strong()
                                        .color(Color32::WHITE),
                                )
                                .fill(Color32::from_rgb(160, 48, 48)),
                            )
                            .clicked()
                        {
                            close_all = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                } else {
                    let first_name = all_names
                        .first()
                        .map(String::as_str)
                        .unwrap_or("your logged-in character");
                    ui.label(format!(
                        "Close this new tab and return to {first_name}, or close all ({names_list})?"
                    ));
                    ui.add_space(12.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Close New Tab")
                                        .strong()
                                        .color(Color32::from_rgb(20, 14, 6)),
                                )
                                .fill(accent),
                            )
                            .clicked()
                        {
                            close_new_tab = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(&close_all_label)
                                        .strong()
                                        .color(Color32::WHITE),
                                )
                                .fill(Color32::from_rgb(160, 48, 48)),
                            )
                            .clicked()
                        {
                            close_all = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                }
            });

        self.close_confirm_open = open && !cancel;
        if close_current_only {
            self.close_confirm_open = false;
            self.close_active_character_tab(ctx);
        } else if close_new_tab {
            self.close_confirm_open = false;
            self.switch_to_bg_session(0, ctx);
        } else if close_all {
            self.quit_all_sessions(ctx);
        }
    }

    fn render_settings_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let accent = self.ui_accent();
        ui.heading(RichText::new("Native Client Settings & Configuration").strong().color(accent));
        ui.add_space(4.0);
        ui.label(RichText::new("Configure your appearance, audio, map behavior, cloud sync, and imported data.").color(Color32::LIGHT_GRAY));
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.label(RichText::new("Dream Window Skin").strong().color(accent));
            ui.horizontal_wrapped(|ui| {
                for t in [
                    DreamTheme::ClassicPurple,
                    DreamTheme::DragonHoard,
                    DreamTheme::KitterMagic,
                    DreamTheme::FerianWylde,
                ] {
                    if ui.radio(!self.colors.custom_ui_bg && self.theme == t, t.label()).clicked() {
                        self.theme = t;
                        self.colors.custom_ui_bg = false;
                        self.save_config();
                    }
                }
            });
            if self.colors.custom_ui_bg {
                ui.label(
                    RichText::new("(Custom window colors below are currently overriding the skin)")
                        .size(11.0)
                        .color(Color32::GRAY),
                );
            }
            ui.separator();
            ui.label(RichText::new("Fonts & Size").strong().color(accent));
            ui.horizontal(|ui| {
                ui.label("Font Family:");
                for fam in ["Proportional", "Monospace", "Fixedsys"] {
                    if ui.radio(self.font_family == fam, fam).clicked() {
                        self.font_family = fam.to_owned();
                        self.save_config();
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Font Size:");
                if ui.add(egui::Slider::new(&mut self.font_size, 10..=22).suffix(" px")).changed() {
                    self.save_config();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Chat text size:");
                if ui.add(egui::Slider::new(&mut self.chat_font_size, 9..=28).suffix(" px")).changed() {
                    self.save_config();
                }
            });
            if ui.checkbox(&mut self.autocorrect, "Auto-correct common typing mistakes when sending").changed() {
                self.save_config();
            }
            ui.horizontal(|ui| {
                ui.label("Left-panel portrait size:");
                if ui
                    .add(egui::Slider::new(&mut self.portrait_size, 48.0..=160.0).suffix(" px"))
                    .changed()
                {
                    self.save_config();
                }
                if ui.small_button("Reset").clicked() {
                    self.portrait_size = 64.0;
                    self.save_config();
                }
            });
            ui.separator();

            // Two `(label, color)` picker entries rendered side-by-side in a 4-column settings grid.
            type ColorPairRow<'a> = ((&'a str, &'a mut Color32), (&'a str, &'a mut Color32));
            fn render_color_pairs_grid(
                ui: &mut egui::Ui,
                id: &str,
                pairs: &mut [ColorPairRow<'_>],
                changed: &mut bool,
            ) {
                egui::Grid::new(id)
                    .num_columns(4)
                    .spacing([14.0, 6.0])
                    .show(ui, |ui| {
                        for ((l1, c1), (l2, c2)) in pairs {
                            ui.label(*l1);
                            if edit_rgb_color(ui, c1) {
                                *changed = true;
                            }
                            ui.label(*l2);
                            if edit_rgb_color(ui, c2) {
                                *changed = true;
                            }
                            ui.end_row();
                        }
                    });
            }

            let mut colors_changed = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Colors").strong().color(accent));
                if ui.small_button("Reset All Colors").clicked() {
                    self.colors = UiColors::default();
                    colors_changed = true;
                }
            });

            let preview = self.colors;
            let (preview_bg, _, preview_accent) = self.effective_colors();
            egui::Frame::none()
                .fill(preview_bg)
                .stroke(Stroke::new(1.0_f32, preview_accent))
                .rounding(4.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Regular chat").color(preview.chat_text));
                        ui.label(RichText::new("[bracket text]").color(preview.bracket_text));
                        ui.label(RichText::new("Your speech").color(preview.my_speech));
                        ui.label(RichText::new("*Emote*").italics().color(preview.emote_text));
                        ui.label(RichText::new("Whisper").color(preview.whisper_text));
                        ui.label(RichText::new("Emit").color(preview.emit_text));
                    });
                    ui.add_space(4.0);
                    let preview_sz = (self.font_size as f32).clamp(11.0, 24.0).round();
                    let preview_regular = FontId::new(preview_sz, self.ui_font_family(false));
                    let preview_bold = FontId::new(preview_sz, self.ui_font_family(true));
                    ui.scope(|ui| {
                        ui.spacing_mut().button_padding = Vec2::new(8.0, 4.0);
                        ui.horizontal_wrapped(|ui| {
                            let _ = ui.add(
                                egui::Button::new(
                                    RichText::new("Active")
                                        .font(preview_bold.clone())
                                        .strong()
                                        .color(preview.tab_active_text),
                                )
                                .fill(preview.tab_active_bg)
                                .rounding(4.0),
                            );
                            let _ = ui.add(
                                egui::Button::new(
                                    RichText::new("Unread (2)")
                                        .font(preview_bold.clone())
                                        .strong()
                                        .color(preview.tab_unread_text),
                                )
                                .fill(preview.tab_unread_bg)
                                .stroke(Stroke::new(1.5_f32, preview.tab_unread_bg))
                                .rounding(4.0),
                            );
                            let _ = ui.add(
                                egui::Button::new(
                                    RichText::new("Inactive")
                                        .font(preview_regular.clone())
                                        .color(preview.tab_inactive_text),
                                )
                                .fill(preview.tab_inactive_bg)
                                .rounding(4.0),
                            );
                        });
                    });
                });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Chat & Message Colors").strong());
                if ui.small_button("Reset Chat").clicked() {
                    self.colors.reset_chat_colors();
                    colors_changed = true;
                }
            });
            let mut chat_pairs = [
                (
                    ("Regular chat:", &mut self.colors.chat_text),
                    ("Your speech:", &mut self.colors.my_speech),
                ),
                (
                    ("[Bracket text]:", &mut self.colors.bracket_text),
                    ("Emotes:", &mut self.colors.emote_text),
                ),
                (
                    ("Whispers & Trade:", &mut self.colors.whisper_text),
                    ("Emits & Links:", &mut self.colors.emit_text),
                ),
                (
                    ("Shouts:", &mut self.colors.shout_text),
                    ("Dice rolls:", &mut self.colors.roll_text),
                ),
                (
                    ("System / Success:", &mut self.colors.success_text),
                    ("Warnings:", &mut self.colors.warning_text),
                ),
                (
                    ("Errors:", &mut self.colors.error_text),
                    ("Typed input:", &mut self.colors.input_text),
                ),
            ];
            render_color_pairs_grid(
                ui,
                "settings_chat_colors_grid",
                &mut chat_pairs,
                &mut colors_changed,
            );

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Tab Colors (Chat, Whisper & Character)").strong());
                if ui.small_button("Reset Tabs").clicked() {
                    self.colors.reset_tab_colors();
                    colors_changed = true;
                }
            });
            let mut tab_pairs = [
                (
                    ("Active tab fill:", &mut self.colors.tab_active_bg),
                    ("Active tab text:", &mut self.colors.tab_active_text),
                ),
                (
                    ("Unread tab fill:", &mut self.colors.tab_unread_bg),
                    ("Unread tab text:", &mut self.colors.tab_unread_text),
                ),
                (
                    ("Inactive tab fill:", &mut self.colors.tab_inactive_bg),
                    ("Inactive tab text:", &mut self.colors.tab_inactive_text),
                ),
            ];
            render_color_pairs_grid(
                ui,
                "settings_tab_colors_grid",
                &mut tab_pairs,
                &mut colors_changed,
            );

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Window & Background Colors").strong());
                if ui.small_button("Copy Current Skin").clicked() {
                    self.colors.chat_bg = self.theme.bg();
                    self.colors.panel_bg = self.theme.panel();
                    self.colors.accent = self.theme.accent();
                    self.colors.custom_ui_bg = true;
                    colors_changed = true;
                }
                if ui.small_button("Reset Window").clicked() {
                    self.colors.reset_window_colors();
                    colors_changed = true;
                }
            });
            if ui
                .checkbox(
                    &mut self.colors.custom_ui_bg,
                    "Use custom background & accent colors",
                )
                .changed()
            {
                colors_changed = true;
            }
            ui.add_enabled_ui(self.colors.custom_ui_bg, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (label, col) in [
                        ("Chat background:", &mut self.colors.chat_bg),
                        ("Panel / sidebar:", &mut self.colors.panel_bg),
                        ("Accent / headings:", &mut self.colors.accent),
                    ] {
                        ui.label(label);
                        if edit_rgb_color(ui, col) {
                            colors_changed = true;
                        }
                    }
                });
            });
            if colors_changed {
                self.save_config();
            }
            ui.separator();
            ui.label(RichText::new("Animations & Map").strong().color(accent));
            ui.horizontal(|ui| {
                for mode in ["Full", "Half", "Off"] {
                    if ui.radio(self.anim_mode == mode, mode).clicked() {
                        self.anim_mode = mode.to_owned();
                        self.save_config();
                    }
                }
            });
            if ui.checkbox(&mut self.show_avatar_names, "Show names above avatars (Tab)").changed() {
                self.save_config();
            }
            if ui.checkbox(&mut self.limit_view_range, "Furcadia Default mode (1× 624 × 352 view)").changed() {
                self.zoom = if self.limit_view_range { 1.0 } else { 1.25 };
                self.save_config();
            }
            if ui.checkbox(&mut self.allow_dream_skins, "Allow dream custom window skins").changed() {
                self.save_config();
            }
            if ui.checkbox(&mut self.show_debug_packets, "Log raw/extended packets to chat").changed() {
                self.save_config();
            }
            ui.separator();
            ui.label(RichText::new("Whispers & Chat Behavior").strong().color(accent));
            if ui
                .checkbox(
                    &mut self.whisper_open_tab_on_incoming,
                    "Automatically open whisper tab on incoming whisper",
                )
                .changed()
            {
                self.save_config();
            }
            if ui
                .checkbox(
                    &mut self.whisper_emote_in_whisper,
                    "Send :emotes inside active whisper tab as whispers",
                )
                .changed()
            {
                self.save_config();
            }
            ui.horizontal(|ui| {
                ui.label("Dice roll expression:");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.dice_roll)
                            .hint_text("1d6")
                            .desired_width(90.0),
                    )
                    .changed()
                {
                    self.save_config();
                }
                for preset in ["1d6", "1d20", "1d100", "2d6"] {
                    if ui.selectable_label(self.dice_roll.trim() == preset, preset).clicked() {
                        self.dice_roll = preset.to_owned();
                        self.save_config();
                    }
                }
            });
            ui.separator();
            ui.label(RichText::new("Audio, Mentions & Taskbar Flash").strong().color(accent));
            if ui.checkbox(&mut self.sound_enabled, "Sound effects (.wav)").changed() { self.save_config(); }
            if ui.checkbox(&mut self.music_enabled, "Background music").changed() { self.save_config(); }
            if ui.checkbox(&mut self.notify_enabled, "Enable desktop notifications").changed() { self.save_config(); }
            if ui.checkbox(&mut self.flash_taskbar_on_message, "Flash taskbar on whispers & mentions (when unfocused)").changed() { self.save_config(); }
            if ui.checkbox(&mut self.flash_taskbar_on_main_chat, "Flash taskbar on any Main Chat message (when unfocused)").changed() { self.save_config(); }
            if ui.checkbox(&mut self.blink_tray_on_alert, "Blink system tray icon on alerts (when unfocused / in tray)").changed() {
                if !self.blink_tray_on_alert {
                    self.tray.set_attention(false);
                }
                self.save_config();
            }
            if ui.checkbox(&mut self.notify_whisper, "Alert on incoming whispers").changed() { self.save_config(); }
            if ui.checkbox(&mut self.notify_pounce, "Alert when Pounce friend comes online").changed() { self.save_config(); }
            if ui.checkbox(&mut self.notify_mention, "Watch Main Chat for your name & nicknames (mentions)").changed() { self.save_config(); }
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label("Nicknames to alert on:");
                let nick_edit = ui.add_enabled(
                    self.notify_mention,
                    egui::TextEdit::singleline(&mut self.mention_nicknames)
                        .hint_text("e.g. Aur, Bear, Amala (comma-separated)")
                        .desired_width(260.0),
                );
                if nick_edit.changed() {
                    self.save_config();
                }
            });
            if ui.checkbox(&mut self.notify_only_unfocused, "Only send desktop notifications when window is unfocused").changed() { self.save_config(); }
            ui.separator();
            ui.label(RichText::new("Pounce & Window Behavior").strong().color(accent));
            if ui.checkbox(&mut self.pounce_visible, "Show Pounce panel").changed() { self.save_config(); }
            if ui.checkbox(&mut self.pounce_detached, "Detach Pounce into separate window").changed() { self.save_config(); }
            if ui.checkbox(&mut self.minimize_to_tray, "Minimize to tray/panel").changed() { self.save_config(); }
            if ui.checkbox(&mut self.close_to_tray, "Close button minimizes instead of quitting").changed() { self.save_config(); }
            if (self.minimize_to_tray || self.close_to_tray) && self.is_wayland {
                ui.label(
                    RichText::new("Running on pure Wayland without XWayland; minimize/hide-to-tray falls back to taskbar minimize.")
                        .size(11.0)
                        .color(Color32::from_rgb(230, 200, 120)),
                );
            }
            ui.separator();
            ui.label(RichText::new("GitHub Updates").strong().color(accent));
            if ui
                .checkbox(&mut self.check_updates_on_startup, "Check GitHub for updates on startup")
                .changed()
            {
                self.save_config();
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.update_checking, egui::Button::new("Check for Updates Now"))
                    .clicked()
                {
                    self.trigger_update_check(ctx);
                }
                if self.update_available.is_some() && ui.button("Install Update...").clicked() {
                    self.update_popup_open = true;
                }
            });
            if let Some(status) = &self.update_status {
                ui.label(RichText::new(status).size(12.0).color(Color32::LIGHT_GRAY));
            }
            ui.separator();
            ui.label(RichText::new("Chat & Whisper Logs").strong().color(accent));
            if ui.checkbox(&mut self.logging_enabled, "Save chat logs").changed() {
                self.save_config();
            }
            ui.add_enabled_ui(self.logging_enabled, |ui| {
                if ui.checkbox(&mut self.log_whispers, "Save whisper logs").changed() {
                    self.save_config();
                }
                ui.horizontal(|ui| {
                    ui.label("Log folders:");
                    if ui
                        .radio(
                            self.shared_logs_mode,
                            "Shared (Furcadia-style, one folder for all characters)",
                        )
                        .clicked()
                    {
                        self.shared_logs_mode = true;
                        self.log_viewer_char = SHARED_LOG_GROUP.to_owned();
                        self.save_config();
                    }
                    if ui
                        .radio(!self.shared_logs_mode, "Separate folder per character")
                        .clicked()
                    {
                        self.shared_logs_mode = false;
                        self.log_viewer_char = self.active_character_wire_name();
                        self.save_config();
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("Keep logs:");
                for (days, label) in [
                    (0u32, "Forever"),
                    (7, "7 days"),
                    (30, "30 days"),
                    (90, "90 days"),
                    (365, "1 year"),
                ] {
                    if ui.radio(self.log_retention_days == days, label).clicked() {
                        self.log_retention_days = days;
                        self.save_config();
                    }
                }
            });
            if let Some(dir) = default_logs_root() {
                ui.label(
                    RichText::new(format!("Log directory: {}", dir.display()))
                        .size(11.0)
                        .color(Color32::GRAY),
                );
            }
            ui.separator();
            ui.label(RichText::new("Sync with a cloud folder").strong().color(accent));
            ui.label(
                RichText::new(
                    "Use a folder already synced by Nextcloud, Google Drive, Dropbox, or another provider. Choose the same cloud folder on each computer.",
                )
                .size(12.0)
                .color(Color32::LIGHT_GRAY),
            );
            ui.label(
                RichText::new("The app creates settings-<device>.ini and logs/<device>/ inside this folder.")
                    .size(11.0)
                    .color(Color32::GRAY),
            );
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.sync_folder_input)
                        .hint_text("Path to a dedicated cloud-synced folder")
                        .desired_width((ui.available_width() - 142.0).max(120.0)),
                );
                if ui.button("Browse...").clicked() {
                    self.browse_sync_folder();
                }
                if ui.button("Use Folder").clicked() {
                    self.apply_sync_folder();
                }
            });
            ui.horizontal(|ui| {
                if ui.small_button("Turn Off Sync").clicked() {
                    self.sync_folder_input.clear();
                    self.apply_sync_folder();
                }
                if !self.sync_folder.is_empty() {
                    ui.label(
                        RichText::new(format!("Active: {}", self.sync_folder))
                            .size(11.0)
                            .color(Color32::GRAY),
                    );
                }
            });
            if let Some(message) = &self.sync_folder_message {
                ui.label(
                    RichText::new(message)
                        .size(11.0)
                        .color(Color32::from_rgb(180, 220, 180)),
                );
            }
            ui.label(
                RichText::new(
                    "Set this folder on the first computer, wait for the provider to sync it, then choose that same cloud folder on each other computer and restart. The newest settings snapshot loads at startup. Login credentials stay local; per-device logs are combined in the Log Viewer.",
                )
                .size(11.0)
                .color(Color32::LIGHT_GRAY),
            );
            if ui.button("Open Log Viewer").clicked() {
                self.open_log_viewer();
            }
            ui.separator();
            ui.label(RichText::new("Cached Downloads").strong().color(accent));
            if self.cache_usage.is_none() {
                self.refresh_cache_usage();
            }
            if let Some(usage) = self.cache_usage {
                let (sf, sb) = usage.species;
                let (df, db) = usage.dreams;
                let (pf, pb) = usage.patches;
                ui.label(
                    RichText::new(format!(
                        "Avatars, portraits & images: {sf} file(s) ({})",
                        format_bytes(sb)
                    ))
                    .size(12.0)
                    .color(Color32::LIGHT_GRAY),
                );
                ui.label(
                    RichText::new(format!("Dream packages: {df} file(s) ({})", format_bytes(db)))
                        .size(12.0)
                        .color(Color32::LIGHT_GRAY),
                );
                ui.label(
                    RichText::new(format!(
                        "Stock game patches: {pf} file(s) ({})",
                        format_bytes(pb)
                    ))
                    .size(12.0)
                    .color(Color32::LIGHT_GRAY),
                );
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("Flush & Redownload Avatars/Portraits").clicked() {
                    self.clear_avatar_download_cache();
                    ctx.request_repaint();
                }
                if ui.button("Flush & Redownload Dreams").clicked() {
                    self.clear_dream_download_cache();
                    ctx.request_repaint();
                }
                if ui.button("Flush All & Redownload").clicked() {
                    self.clear_all_download_caches();
                    ctx.request_repaint();
                }
            });
            ui.horizontal(|ui| {
                if ui.small_button("Refresh").clicked() {
                    self.refresh_cache_usage();
                }
                if ui.small_button("Open Cache Folder").clicked() {
                    if let Some(dir) = AssetLocator::cache_root_dir() {
                        let _ = std::fs::create_dir_all(&dir);
                        #[cfg(target_os = "windows")]
                        let _ = furcadia_client::sys::command("explorer").arg(&dir).spawn();
                        #[cfg(target_os = "macos")]
                        let _ = std::process::Command::new("open").arg(&dir).spawn();
                        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                        let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                    }
                }
            });
            ui.label(
                RichText::new(
                    "Deletes downloaded species, custom portraits, chat images, dream packages, and stock game patches so they download fresh. Your settings, logs, and imported Windows Furcadia assets are kept.",
                )
                .size(11.0)
                .color(Color32::GRAY),
            );
            if let Some(report) = &self.cache_report {
                ui.colored_label(Color32::from_rgb(130, 225, 145), report);
            }
            ui.separator();
            ui.label(RichText::new("Windows Importer").strong().color(accent));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.import_path_input).desired_width(240.0));
                if ui.button("Browse...").clicked() { self.browse_import_directory(); }
            });
            if ui.button("Import Windows Furcadia Data").clicked() {
                self.run_windows_import();
            }
            if let Some(report) = &self.import_report {
                ui.colored_label(Color32::from_rgb(130, 225, 145), report);
            }
        });
    }

fn render_log_viewer_window(&mut self, ctx: &egui::Context) {
        let mut open = self.log_viewer_open;
        let accent = self.ui_accent();
        egui::Window::new("Furcadia Chat & Whisper Logs")
            .open(&mut open)
            .resizable(true)
            .default_size(Vec2::new(660.0, 440.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .checkbox(&mut self.logging_enabled, "Logging enabled")
                        .changed()
                    {
                        self.save_config();
                    }
                    if ui.button("Refresh").clicked() {
                        self.refresh_log_viewer_files();
                    }
                    if ui.button("Open Logs Folder").clicked() {
                        if let Some(dir) = default_logs_root() {
                            let _ = std::fs::create_dir_all(&dir);
                            #[cfg(target_os = "windows")]
                            let _ = furcadia_client::sys::command("explorer").arg(&dir).spawn();
                            #[cfg(target_os = "macos")]
                            let _ = std::process::Command::new("open").arg(&dir).spawn();
                            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                            let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                        }
                    }
                });
                ui.separator();

                let chars = default_logs_root()
                    .map(|d| list_log_characters(&d))
                    .unwrap_or_default();
                if chars.is_empty() {
                    ui.label(
                        RichText::new("No saved logs yet. Chat or whisper while logging is enabled.")
                            .italics()
                            .color(Color32::GRAY),
                    );
                    return;
                }

                ui.horizontal_wrapped(|ui| {
                    ui.label("Logs:");
                    for ch in &chars {
                        if ui.selectable_label(&self.log_viewer_char == ch, ch).clicked() {
                            self.log_viewer_char = ch.clone();
                            self.refresh_log_viewer_files();
                        }
                    }
                });
                ui.separator();

                let avail_h = ui.available_height().max(180.0);
                ui.columns(2, |cols| {
                    cols[0].set_max_width(210.0);
                    egui::ScrollArea::vertical()
                        .id_salt("log_files_list")
                        .max_height(avail_h)
                        .show(&mut cols[0], |ui| {
                            let mut picked = None;
                            for path in &self.log_viewer_files {
                                let label = path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("log.txt");
                                let is_whisper = path
                                    .parent()
                                    .and_then(|p| p.file_name())
                                    .and_then(|n| n.to_str())
                                    .is_some_and(|n| n == "whispers");
                                let display = if is_whisper {
                                    format!("[W] {label}")
                                } else {
                                    label.to_owned()
                                };
                                let sel = self.log_viewer_selected.as_ref() == Some(path);
                                if ui.selectable_label(sel, display).clicked() {
                                    picked = Some(path.clone());
                                }
                            }
                            if let Some(p) = picked {
                                self.load_log_file_into_viewer(p);
                            }
                        });

                    egui::ScrollArea::vertical()
                        .id_salt("log_file_body")
                        .max_height(avail_h)
                        .show(&mut cols[1], |ui| {
                            ui.label(
                                RichText::new(
                                    self.log_viewer_selected
                                        .as_ref()
                                        .and_then(|p| p.file_name())
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("Select a log file"),
                                )
                                .strong()
                                .color(accent),
                            );
                            ui.add(
                                egui::TextEdit::multiline(&mut self.log_viewer_content.as_str())
                                    .desired_width(f32::INFINITY)
                                    .font(egui::TextStyle::Monospace),
                            );
                        });
                });
            });
        self.log_viewer_open = open;
    }

    fn render_bg_session_tab_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, current_tab_label: &str) {
        if self.bg_sessions.is_empty() {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            let mut switch_to: Option<usize> = None;
            for (idx, bg) in self.bg_sessions.iter().enumerate() {
                let lbl = if bg.unread_count > 0 {
                    format!("{} ({})", bg.char_name, bg.unread_count)
                } else {
                    bg.char_name.clone()
                };
                let clicked = if bg.unread_count > 0 {
                    ui.add(
                        egui::Button::new(
                            RichText::new(lbl)
                                .strong()
                                .color(Color32::from_rgb(20, 14, 6)),
                        )
                        .fill(Color32::from_rgb(235, 180, 60))
                        .stroke(Stroke::new(1.5_f32, Color32::from_rgb(255, 225, 130))),
                    )
                    .clicked()
                } else {
                    ui.selectable_label(false, RichText::new(lbl).size(12.0)).clicked()
                };
                if clicked {
                    switch_to = Some(idx);
                }
            }
            let _ = ui.selectable_label(true, RichText::new(current_tab_label).size(12.0).strong());
            if ui
                .small_button("×")
                .on_hover_text("Close this tab and return to your logged-in character")
                .clicked()
            {
                switch_to = Some(0);
            }
            if let Some(idx) = switch_to {
                self.switch_to_bg_session(idx, ctx);
            }
        });
        ui.separator();
    }

    fn render_login_screen(&mut self, ctx: &egui::Context) {
        if !self.bg_sessions.is_empty() {
            egui::TopBottomPanel::top("login_tab_bar")
                .frame(egui::Frame::none().fill(BG_PURPLE).inner_margin(8.0))
                .show(ctx, |ui| {
                    self.render_bg_session_tab_bar(ui, ctx, "Login (+Char Tab)");
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG_PURPLE))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.label(
                        RichText::new("FURCADIA")
                            .size(44.0)
                            .strong()
                            .color(GOLD),
                    );
                    ui.label(
                        RichText::new("The Second Dreaming  •  Native Furcadia Client")
                            .size(16.0)
                            .italics()
                            .color(Color32::from_rgb(225, 200, 145)),
                    );
                    ui.add_space(28.0);

                    egui::Frame::none()
                        .fill(CARD_PURPLE)
                        .stroke(Stroke::new(2.0_f32, GOLD_DIM))
                        .rounding(8.0)
                        .inner_margin(24.0)
                        .show(ui, |ui| {
                            ui.set_width(360.0);
                            ui.label(RichText::new("Email Address").color(GOLD));
                            let email_resp = ui.add(
                                egui::TextEdit::singleline(&mut self.email)
                                    .hint_text("you@example.com")
                                    .desired_width(350.0),
                            );
                            ui.add_space(10.0);

                            ui.label(RichText::new("Password").color(GOLD));
                            let pass_resp = ui.add(
                                egui::TextEdit::singleline(&mut self.password)
                                    .password(true)
                                    .desired_width(350.0),
                            );
                            let enter_pressed = (email_resp.lost_focus() || pass_resp.lost_focus())
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));

                            ui.add_space(8.0);
                            ui.colored_label(
                                Color32::from_rgb(255, 190, 90),
                                "Connection warning: game traffic uses legacy TLS. Furcadia's server certificate is expired and is not verified, so an active attacker could impersonate the server and capture your password.",
                            );

                            ui.add_space(16.0);
                            let login_btn = egui::Button::new(
                                RichText::new(if self.auth_busy {
                                    "LOGGING IN..."
                                } else {
                                    "LOGIN"
                                })
                                .size(18.0)
                                .strong()
                                .color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgb(56, 38, 110))
                            .stroke(Stroke::new(1.5_f32, GOLD))
                            .min_size(Vec2::new(350.0, 40.0));

                            if ui.add_enabled(!self.auth_busy, login_btn).clicked()
                                || (enter_pressed && !self.auth_busy)
                            {
                                self.trigger_login();
                            }

                            if let Some(err) = &self.auth_error {
                                ui.add_space(8.0);
                                ui.colored_label(Color32::from_rgb(245, 110, 110), err);
                            }

                            ui.add_space(12.0);
                            let mut changed = false;
                            changed |= ui
                                .checkbox(&mut self.remember_email, "Remember my email address")
                                .changed();
                            changed |= ui
                                .checkbox(&mut self.remember_password, "Remember my password")
                                .changed();
                            changed |= ui
                                .checkbox(&mut self.auto_login, "Log in automatically")
                                .changed();
                            if changed {
                                self.save_config();
                            }

                            ui.add_space(12.0);
                            ui.separator();
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.hyperlink_to(
                                    RichText::new("Create Account").color(GOLD),
                                    "https://www.furcadia.com/create/",
                                );
                                ui.label("•");
                                ui.hyperlink_to(
                                    RichText::new("Forgot Password?").color(GOLD),
                                    "https://www.furcadia.com/accounts/forgot.php",
                                );
                                ui.label("•");
                                ui.hyperlink_to(
                                    RichText::new("User Agreement").color(GOLD),
                                    "https://www.furcadia.com/about/tos.html",
                                );
                            });
                        });

                    ui.add_space(20.0);
                    ui.label(
                        RichText::new("Let your imagination soar!  •  \"Teen+\" Environment")
                            .size(12.0)
                            .color(Color32::from_rgb(175, 165, 200)),
                    );
                    ui.add_space(12.0);
                    if let Some(tex) = &self.bear_logo {
                        ui.add(
                            egui::Image::new(tex)
                                .max_size(Vec2::splat(72.0))
                                .rounding(6.0),
                        );
                    }
                    ui.add_space(4.0);
                    if ui
                        .small_button("About & Open Source Credits")
                        .clicked()
                    {
                        self.about_open = !self.about_open;
                    }
                });
            });
    }

    fn render_character_select_screen(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("char_select_top")
            .frame(egui::Frame::none().fill(BG_PURPLE).inner_margin(8.0))
            .show(ctx, |ui| {
                self.render_bg_session_tab_bar(ui, ctx, "New Char (+Char Tab)");
                ui.horizontal(|ui| {
                    if ui.button("← Switch Account").clicked() {
                        self.screen = AppScreen::Login;
                    }
                    ui.separator();
                    ui.label(
                        RichText::new(format!("Account: {}", self.email))
                            .color(GOLD)
                            .strong(),
                    );
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG_PURPLE).inner_margin(12.0))
            .show(ctx, |ui| {
                ui.columns(2, |cols| {
                    // Left Column: Announcements
                    egui::Frame::none()
                        .fill(CARD_PURPLE)
                        .stroke(Stroke::new(1.5_f32, GOLD_DIM))
                        .rounding(8.0)
                        .inner_margin(12.0)
                        .show(&mut cols[0], |ui| {
                            ui.heading(RichText::new("Announcements").color(GOLD));
                            ui.separator();
                            egui::ScrollArea::vertical()
                                .id_salt("announcements_scroll")
                                .show(ui, |ui| {
                                    if self.news.is_empty() {
                                        ui.label("Loading Furcadia announcements...");
                                    } else {
                                        for entry in &self.news {
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    RichText::new(&entry.title)
                                                        .strong()
                                                        .color(Color32::from_rgb(230, 225, 245)),
                                                );
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(egui::Align::Center),
                                                    |ui| {
                                                        ui.label(
                                                            RichText::new(&entry.date)
                                                                .size(11.0)
                                                                .color(Color32::LIGHT_GRAY),
                                                        );
                                                    },
                                                );
                                            });
                                            ui.label(
                                                RichText::new(&entry.body)
                                                    .color(Color32::from_rgb(200, 195, 220)),
                                            );
                                            if let Some(url) = &entry.url {
                                                ui.hyperlink_to(
                                                    RichText::new("READ MORE").color(GOLD).strong(),
                                                    url,
                                                );
                                            }
                                            ui.add_space(6.0);
                                            ui.separator();
                                            ui.add_space(4.0);
                                        }
                                    }
                                });
                        });

                    // Right Column: Character Selection & Edit
                    egui::Frame::none()
                        .fill(CARD_PURPLE)
                        .stroke(Stroke::new(1.5_f32, GOLD_DIM))
                        .rounding(8.0)
                        .inner_margin(12.0)
                        .show(&mut cols[1], |ui| {
                            ui.horizontal(|ui| {
                                ui.heading(RichText::new("Select Character").color(GOLD));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.radio_value(&mut self.char_sort, CharSort::Alphabetical, "A-Z");
                                        ui.radio_value(&mut self.char_sort, CharSort::Recent, "Recent");
                                        if ui
                                            .add_enabled(!self.auth_busy, egui::Button::new("↻ Refresh"))
                                            .on_hover_text("Fetch the latest character list from your account")
                                            .clicked()
                                        {
                                            self.refresh_character_list();
                                        }
                                    },
                                );
                            });
                            ui.separator();

                            let mut sorted_indices: Vec<usize> = self
                                .account_info
                                .as_ref()
                                .map(|i| (0..i.characters.len()).collect())
                                .unwrap_or_default();

                            if let Some(info) = &self.account_info {
                                let saved_norm = self.saved_character.trim().replace('|', " ");
                                match self.char_sort {
                                    CharSort::Recent => sorted_indices.sort_by(|&a, &b| {
                                        let a_saved = !saved_norm.is_empty()
                                            && info.characters[a]
                                                .name
                                                .replace('|', " ")
                                                .eq_ignore_ascii_case(&saved_norm);
                                        let b_saved = !saved_norm.is_empty()
                                            && info.characters[b]
                                                .name
                                                .replace('|', " ")
                                                .eq_ignore_ascii_case(&saved_norm);
                                        b_saved.cmp(&a_saved).then_with(|| {
                                            info.characters[b]
                                                .last_login
                                                .cmp(&info.characters[a].last_login)
                                        })
                                    }),
                                    CharSort::Alphabetical => sorted_indices.sort_by(|&a, &b| {
                                        info.characters[a]
                                            .name
                                            .to_ascii_lowercase()
                                            .cmp(&info.characters[b].name.to_ascii_lowercase())
                                    }),
                                }
                            }

                            let tiles: Vec<(usize, String, String, String, Option<u32>)> = self
                                .account_info
                                .as_ref()
                                .map(|info| {
                                    sorted_indices
                                        .iter()
                                        .map(|&i| {
                                            let c = &info.characters[i];
                                            (
                                                i,
                                                c.id.clone(),
                                                c.name.replace('|', " "),
                                                c.colors.clone(),
                                                c.last_port,
                                            )
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let mut clicked_idx: Option<usize> = None;
                            let mut play_idx: Option<usize> = None;
                            egui::Frame::none()
                                .fill(INPUT_PURPLE)
                                .stroke(Stroke::new(1.0_f32, GOLD_DIM))
                                .rounding(6.0)
                                .inner_margin(6.0)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    egui::ScrollArea::vertical()
                                        .id_salt("char_list_scroll")
                                        .max_height(300.0)
                                        .min_scrolled_height(300.0)
                                        .show(ui, |ui| {
                                            if self.account_info.is_none() {
                                                ui.label("Loading characters...");
                                                return;
                                            }
                                            if tiles.is_empty() {
                                                ui.label("No characters found on this account.");
                                                return;
                                            }
                                            ui.horizontal_wrapped(|ui| {
                                                ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                                                for (idx, id, name, colors, last_port) in &tiles {
                                                    let selected = *idx == self.selected_char_idx;
                                                    let portrait = self.account_character_portrait(
                                                        ctx,
                                                        id,
                                                        name,
                                                        colors,
                                                        *last_port,
                                                    );
                                                    let tile_size = Vec2::new(112.0, 134.0);
                                                    let (rect, resp) =
                                                        ui.allocate_exact_size(tile_size, Sense::click());
                                                    let painter = ui.painter_at(rect);
                                                    let bg = if selected {
                                                        Color32::from_rgb(78, 62, 140)
                                                    } else if resp.hovered() {
                                                        Color32::from_rgb(48, 36, 92)
                                                    } else {
                                                        Color32::TRANSPARENT
                                                    };
                                                    painter.rect_filled(rect, 4.0, bg);
                                                    let img_rect = Rect::from_min_size(
                                                        rect.min + Vec2::new(8.0, 4.0),
                                                        Vec2::new(96.0, 96.0),
                                                    );
                                                    if let Some(tex) = portrait {
                                                        painter.image(
                                                            tex.id(),
                                                            img_rect,
                                                            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                                            Color32::WHITE,
                                                        );
                                                    } else {
                                                        painter.rect_filled(
                                                            img_rect,
                                                            4.0,
                                                            Color32::from_rgb(40, 30, 76),
                                                        );
                                                        painter.text(
                                                            img_rect.center(),
                                                            egui::Align2::CENTER_CENTER,
                                                            name.chars().next().unwrap_or('?'),
                                                            FontId::proportional(36.0),
                                                            GOLD_DIM,
                                                        );
                                                    }
                                                    if selected {
                                                        painter.rect_stroke(
                                                            img_rect,
                                                            2.0,
                                                            Stroke::new(1.5_f32, GOLD),
                                                        );
                                                    }
                                                    let mut shown = name.clone();
                                                    if shown.chars().count() > 13 {
                                                        shown = shown.chars().take(12).collect::<String>() + "…";
                                                    }
                                                    painter.text(
                                                        Pos2::new(rect.center().x, rect.max.y - 16.0),
                                                        egui::Align2::CENTER_CENTER,
                                                        shown,
                                                        FontId::proportional(14.0),
                                                        if selected {
                                                            Color32::WHITE
                                                        } else {
                                                            Color32::from_rgb(200, 190, 235)
                                                        },
                                                    );
                                                    let resp = resp.on_hover_text(name.as_str());
                                                    if resp.double_clicked() {
                                                        play_idx = Some(*idx);
                                                    } else if resp.clicked() {
                                                        clicked_idx = Some(*idx);
                                                    }
                                                }
                                            });
                                        });
                                });
                            if let Some(idx) = clicked_idx.or(play_idx) {
                                self.selected_char_idx = idx;
                                if let Some(ch) =
                                    self.account_info.as_ref().and_then(|i| i.characters.get(idx))
                                {
                                    self.edit_desc_buf = ch.desc.clone();
                                }
                                self.editing_char = false;
                            }
                            if play_idx.is_some() {
                                self.trigger_play();
                            }

                            ui.add_space(6.0);
                            ui.columns(3, |c| {
                                let mk = |t: &str| {
                                    egui::Button::new(RichText::new(t).strong().size(15.0).color(Color32::WHITE))
                                        .fill(Color32::from_rgb(44, 30, 90))
                                        .stroke(Stroke::new(1.0_f32, GOLD_DIM))
                                        .min_size(Vec2::new(0.0, 32.0))
                                };
                                if c[0]
                                    .add_sized([c[0].available_width(), 32.0], mk("NEW"))
                                    .on_hover_text("Create a new character (web editor)")
                                    .clicked()
                                {
                                    ctx.open_url(egui::OpenUrl::new_tab("https://www.furcadia.com/fured"));
                                }
                                if c[1]
                                    .add_sized([c[1].available_width(), 32.0], mk("MY DEN"))
                                    .on_hover_text("View your characters on My Den")
                                    .clicked()
                                {
                                    ctx.open_url(egui::OpenUrl::new_tab("https://cms.furcadia.com/myden"));
                                }
                                if c[2]
                                    .add_sized([c[2].available_width(), 32.0], mk("EDIT"))
                                    .on_hover_text("Edit the selected character's description")
                                    .clicked()
                                {
                                    self.editing_char = !self.editing_char;
                                }
                            });

                            if self.editing_char {
                                ui.add_space(6.0);
                                egui::Frame::none()
                                    .fill(INPUT_PURPLE)
                                    .stroke(Stroke::new(1.0_f32, GOLD_DIM))
                                    .rounding(6.0)
                                    .inner_margin(8.0)
                                    .show(ui, |ui| {
                                        ui.label(RichText::new("Edit Character Description").color(GOLD));
                                        ui.add(
                                            egui::TextEdit::multiline(&mut self.edit_desc_buf)
                                                .desired_rows(3)
                                                .desired_width(f32::INFINITY),
                                        );
                                        ui.horizontal(|ui| {
                                            if ui.button("Apply on Login").clicked() {
                                                let new_desc = self.edit_desc_buf.trim().to_owned();
                                                self.pending_desc = Some(new_desc.clone());
                                                if let Some(info) = self.account_info.as_mut() {
                                                    if let Some(ch) =
                                                        info.characters.get_mut(self.selected_char_idx)
                                                    {
                                                        ch.desc = new_desc;
                                                    }
                                                }
                                                self.editing_char = false;
                                            }
                                            if ui.button("Cancel").clicked() {
                                                self.editing_char = false;
                                            }
                                        });
                                    });
                            }

                            ui.add_space(12.0);
                            let play_btn = egui::Button::new(
                                RichText::new("PLAY")
                                    .size(22.0)
                                    .strong()
                                    .color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgb(56, 38, 110))
                            .stroke(Stroke::new(1.5_f32, GOLD))
                            .min_size(Vec2::new(ui.available_width(), 44.0));

                            if ui
                                .add_enabled(self.selected_character().is_some(), play_btn)
                                .clicked()
                            {
                                self.trigger_play();
                            }

                            ui.add_space(12.0);
                            ui.label(RichText::new("Select Costume").color(GOLD).size(15.0));
                            egui::Frame::none()
                                .fill(INPUT_PURPLE)
                                .stroke(Stroke::new(1.0_f32, GOLD_DIM))
                                .rounding(6.0)
                                .inner_margin(8.0)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label("Last appearance");
                                    if let Some(ch) = self.selected_character() {
                                        ui.label(
                                            RichText::new(format!("Colors: {}", ch.colors))
                                                .size(11.0)
                                                .color(Color32::LIGHT_GRAY),
                                        );
                                    }
                                });
                        });
                });
            });
    }

    fn render_connecting_screen(&mut self, ctx: &egui::Context) {
        let char_name = self
            .selected_character()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "Character".to_owned());

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG_PURPLE))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(96.0);
                    egui::Frame::none()
                        .fill(CARD_PURPLE)
                        .stroke(Stroke::new(2.0_f32, GOLD_DIM))
                        .rounding(10.0)
                        .inner_margin(28.0)
                        .show(ui, |ui| {
                            ui.set_width(460.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new("FURCADIA")
                                        .size(42.0)
                                        .strong()
                                        .color(GOLD),
                                );
                                ui.label(
                                    RichText::new("The Second Dreaming")
                                        .size(14.0)
                                        .italics()
                                        .color(Color32::from_rgb(225, 200, 145)),
                                );
                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(10.0);
                                ui.label(
                                    RichText::new(format!("Joining as {char_name}"))
                                        .size(16.0)
                                        .strong()
                                        .color(Color32::from_rgb(230, 225, 245)),
                                );
                                ui.add_space(16.0);

                                if let Some(err) = self.connecting_error.clone() {
                                    let suggest_web_login = err.to_ascii_lowercase().contains("id error")
                                        || err.contains("(6)");
                                    egui::Frame::none()
                                        .fill(INPUT_PURPLE)
                                        .stroke(Stroke::new(
                                            1.0_f32,
                                            Color32::from_rgb(220, 90, 90),
                                        ))
                                        .rounding(6.0)
                                        .inner_margin(12.0)
                                        .show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.colored_label(
                                                Color32::from_rgb(245, 115, 115),
                                                err,
                                            );
                                        });
                                    if suggest_web_login {
                                        ui.add_space(8.0);
                                        ui.label(
                                            RichText::new(
                                                "If this ID error keeps happening, sign in to the web client once, then return here and retry.",
                                            )
                                            .size(13.0)
                                            .color(Color32::LIGHT_GRAY),
                                        );
                                        ui.hyperlink_to(
                                            RichText::new("Open Furcadia Web Client").color(GOLD),
                                            "https://www.furcadia.com/fured",
                                        );
                                    }
                                    ui.add_space(16.0);
                                    ui.horizontal(|ui| {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new("Retry Connection")
                                                        .strong()
                                                        .color(Color32::WHITE),
                                                )
                                                .fill(Color32::from_rgb(56, 38, 110))
                                                .stroke(Stroke::new(1.5_f32, GOLD))
                                                .min_size(Vec2::new(170.0, 36.0)),
                                            )
                                            .clicked()
                                        {
                                            self.trigger_play();
                                        }
                                        if ui
                                            .add(
                                                egui::Button::new("← Back to Characters")
                                                    .min_size(Vec2::new(170.0, 36.0)),
                                            )
                                            .clicked()
                                        {
                                            self.connecting_error = None;
                                            self.screen = AppScreen::CharacterSelect;
                                        }
                                    });
                                } else {
                                    let stage_text = if self.connecting_stage.is_empty() {
                                        "Connecting to Furcadia...".to_owned()
                                    } else {
                                        self.connecting_stage.clone()
                                    };
                                    ui.label(
                                        RichText::new(stage_text)
                                            .size(14.0)
                                            .color(Color32::LIGHT_GRAY),
                                    );
                                    ui.add_space(10.0);

                                    let t = ui.input(|i| i.time) as f32;
                                    let bar_w = 380.0_f32;
                                    let (bar_rect, _) = ui.allocate_exact_size(
                                        Vec2::new(bar_w, 12.0),
                                        Sense::hover(),
                                    );
                                    let painter = ui.painter();
                                    painter.rect_filled(bar_rect, 4.0, INPUT_PURPLE);
                                    painter.rect_stroke(
                                        bar_rect,
                                        4.0,
                                        Stroke::new(1.0_f32, GOLD_DIM),
                                    );
                                    let seg_w = if self.is_connected {
                                        bar_rect.width() * 0.85
                                    } else {
                                        bar_rect.width() * 0.28
                                    };
                                    let travel = (bar_rect.width() - seg_w - 4.0).max(0.0);
                                    let phase = ((t * 0.9).sin() * 0.5) + 0.5;
                                    let x0 = bar_rect.min.x + 2.0 + travel * phase;
                                    let fill_rect = Rect::from_min_size(
                                        Pos2::new(x0, bar_rect.min.y + 2.0),
                                        Vec2::new(seg_w, bar_rect.height() - 4.0),
                                    );
                                    painter.rect_filled(fill_rect, 3.0, GOLD);

                                    ui.add_space(16.0);
                                    if ui
                                        .add(
                                            egui::Button::new("Cancel")
                                                .min_size(Vec2::new(140.0, 32.0)),
                                        )
                                        .clicked()
                                    {
                                        self.connect_seq = self.connect_seq.wrapping_add(1);
                                        self.is_connected = false;
                                        self.connected_at = None;
                                        let _ = self.to_net.send(UiToNet::Disconnect);
                                        self.screen = AppScreen::CharacterSelect;
                                    }
                                }
                            });
                        });
                });
            });
    }

    fn send_raw_cmd(&self, cmd: &str) {
        if self.is_connected {
            let wire = format_raw_cmd_wire(cmd);
            let _ = self.to_net.send(UiToNet::Send(ClientCommand::Raw(wire)));
        }
    }

    fn request_player_look(&mut self, target: &str) {
        if !self.is_connected {
            return;
        }
        let target = wire_player_name(target);
        if target.is_empty() {
            return;
        }
        let target_key = shortname_key(&target);
        self.pending_silent_tab_glooks
            .retain(|e| e.target_key != target_key);
        self.silent_self_glook = false;
        self.pending_glook_target = Some((target.clone(), Instant::now()));
        let _ = self.to_net.send(UiToNet::Send(ClientCommand::Glook(target)));
    }

    fn render_pounce_contents(&mut self, ui: &mut egui::Ui, is_detached: bool) {
        let accent = self.ui_accent();
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Pounce").color(accent));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if is_detached {
                    if ui.small_button("Dock").clicked() {
                        self.pounce_detached = false;
                        self.pounce_visible = true;
                    }
                } else {
                    if ui.small_button("×").on_hover_text("Hide Pounce").clicked() {
                        self.pounce_visible = false;
                    }
                    if ui
                        .small_button("↗ Detach")
                        .on_hover_text("Pop out Pounce window")
                        .clicked()
                    {
                        self.pounce_detached = true;
                    }
                }
            });
        });

        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let watch_clicked = ui.button("Watch").clicked();
                let input_w = (ui.available_width() - 10.0).max(60.0);
                let enter = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.watch_input)
                            .hint_text("Character name")
                            .desired_width(input_w),
                    )
                    .lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if watch_clicked || enter {
                    let name = self.watch_input.trim().replace('|', " ");
                    if !name.is_empty() {
                        let wire = wire_player_name(&name);
                        self.pounce.add_watch(name);
                        self.watch_input.clear();
                        self.save_config();
                        if self.is_connected {
                            self.send_raw_cmd(&format!("pounce adduser {wire}"));
                        }
                    }
                }
            });
        });

        let rows: Vec<(String, OnlineStatus)> = self
            .pounce
            .sorted_entries()
            .into_iter()
            .map(|e| (e.name.clone(), e.status))
            .collect();
        let online_count = rows
            .iter()
            .filter(|(_, st)| *st == OnlineStatus::Online)
            .count();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{online_count}/{} online", rows.len()))
                    .size(12.0)
                    .color(Color32::LIGHT_GRAY),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .checkbox(&mut self.pounce_show_offline, RichText::new("Show Offline").size(12.0))
                    .on_hover_text("Show or hide offline furres in Pounce")
                    .changed()
                {
                    self.save_config();
                }
            });
        });
        ui.separator();

        let mut to_remove = None;
        let mut to_glook = None;
        let mut to_whisper = None;
        let mut ctx_action: Option<(String, String, PlayerMenuAction)> = None;
        egui::ScrollArea::vertical()
            .id_salt("pounce_scroll")
            .show(ui, |ui| {
                for (name, status) in rows {
                    if !self.pounce_show_offline && status != OnlineStatus::Online {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("×").clicked() {
                                to_remove = Some(name.clone());
                            }
                            if ui.small_button("💬").on_hover_text("Whisper").clicked() {
                                to_whisper = Some(name.clone());
                            }
                            if self.is_connected && ui.small_button("Look").clicked() {
                                to_glook = Some(wire_player_name(&name));
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                let (dot_color, status_tip) = match status {
                                    OnlineStatus::Online => (Color32::from_rgb(80, 220, 110), "Online"),
                                    OnlineStatus::Offline => (Color32::from_rgb(130, 130, 135), "Offline"),
                                    OnlineStatus::Unknown => {
                                        (Color32::from_rgb(220, 190, 80), "Checking status")
                                    }
                                };
                                ui.colored_label(dot_color, "●").on_hover_text(status_tip);
                                let name_color = if status == OnlineStatus::Online {
                                    Color32::WHITE
                                } else {
                                    Color32::LIGHT_GRAY
                                };
                                let name_w = (ui.available_width() - 4.0).max(32.0);
                                let row_h = ui.text_style_height(&egui::TextStyle::Body).max(18.0).ceil();
                                let name_resp = ui
                                    .allocate_ui(Vec2::new(name_w, row_h), |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(name_display_clean(&name)).color(name_color),
                                            )
                                            .truncate()
                                            .sense(Sense::click()),
                                        )
                                    })
                                    .inner
                                    .on_hover_text(format!("{name}\nRight-click for player actions"));
                                if name_resp.double_clicked() {
                                    to_whisper = Some(name.clone());
                                }
                                let row_name = name.clone();
                                name_resp.context_menu(|ui| {
                                    if let Some(act) = render_player_menu_buttons(ui, false, true, true) {
                                        ctx_action =
                                            Some((row_name.clone(), wire_player_name(&row_name), act));
                                        ui.close_menu();
                                    }
                                });
                            });
                        });
                    });
                }
            });
        if let Some((disp, short, act)) = ctx_action {
            self.execute_player_action(&disp, &short, act);
        }
        if let Some(name) = to_remove {
            let wire = wire_player_name(&name);
            self.pounce.remove_watch(&name);
            self.save_config();
            if self.is_connected {
                self.send_raw_cmd(&format!("pounce remuser {wire}"));
            }
        }
        if let Some(name) = to_glook {
            self.request_player_look(&name);
        }
        if let Some(name) = to_whisper {
            self.open_whisper_tab(&name, true);
        }

        ui.add_space(6.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Saved Dreams").strong().color(accent));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("+ Current")
                    .on_hover_text("Save current dream to Pounce list")
                    .clicked()
                {
                    if let Some(cur) = self
                        .current_dream_url
                        .clone()
                        .or_else(|| self.world.map_name.clone())
                    {
                        self.bookmark_dream(&cur);
                    }
                }
            });
        });
        let mut go_dream: Option<String> = None;
        let mut del_dream: Option<usize> = None;
        for (i, d) in self.dream_bookmarks.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("×").on_hover_text("Remove dream").clicked() {
                        del_dream = Some(i);
                    }
                    let label_w = (ui.available_width() - 4.0).max(40.0);
                    let btn_h = (ui.text_style_height(&egui::TextStyle::Button) + 6.0).max(22.0).ceil();
                    if ui
                        .add_sized([label_w, btn_h], egui::Button::new(d.as_str()).truncate())
                        .on_hover_text(format!("Click to enter {d}"))
                        .clicked()
                    {
                        go_dream = Some(d.clone());
                    }
                });
            });
        }
        if let Some(d) = go_dream {
            self.send_raw_cmd(&format!("fdl {d}"));
        }
        if let Some(idx) = del_dream {
            if idx < self.dream_bookmarks.len() {
                self.dream_bookmarks.remove(idx);
                self.save_config();
            }
        }
    }

    fn render_detached_viewports(&mut self, ctx: &egui::Context) {
        let self_name = self.active_character_name();
        let self_name_key = shortname_key(&self_name);
        if self.pounce_detached {
            let pounce_id = egui::ViewportId::from_hash_of("furc_pounce_viewport");
            let builder = egui::ViewportBuilder::default()
                .with_title("Furcadia - Pounce")
                .with_inner_size([280.0, 420.0]);
            ctx.show_viewport_immediate(pounce_id, builder, |v_ctx, _class| {
                if v_ctx.input(|i| i.viewport().close_requested()) {
                    self.pounce_detached = false;
                }
                let pounce_panel = self.ui_panel();
                egui::CentralPanel::default()
                    .frame(egui::Frame::none().fill(pounce_panel).inner_margin(10.0))
                    .show(v_ctx, |ui| {
                        self.render_pounce_contents(ui, true);
                    });
            });
        }

        let detached_peers: Vec<String> = self
            .whispers
            .iter()
            .filter(|w| w.detached)
            .map(|w| w.peer.clone())
            .collect();
        let player_badges = if detached_peers.is_empty() {
            HashMap::new()
        } else {
            self.current_player_badge_textures(ctx)
        };
        let win_bg = self.ui_bg();
        let win_accent = self.ui_accent();
        let win_colors = self.colors;

        for peer in detached_peers {
            let vp_id = egui::ViewportId::from_hash_of(("furc_whisper", &peer));
            let builder = egui::ViewportBuilder::default()
                .with_title(format!("Whisper: {peer}"))
                .with_inner_size([420.0, 300.0]);
            let mut dock_back = false;
            let mut close_win = false;
            let mut send_msg: Option<String> = None;
            let mut chat_action: Option<ChatClickAction> = None;

            ctx.show_viewport_immediate(vp_id, builder, |v_ctx, _class| {
                if v_ctx.input(|i| i.viewport().close_requested()) {
                    dock_back = true;
                }
                if v_ctx.input(|i| {
                    (i.modifiers.ctrl || i.modifiers.command)
                        && !i.modifiers.shift
                        && i.key_pressed(egui::Key::W)
                }) {
                    close_win = true;
                }
                if let Some(w) = self
                    .whispers
                    .iter_mut()
                    .find(|w| w.peer.eq_ignore_ascii_case(&peer))
                {
                    w.unread = 0;
                    egui::CentralPanel::default()
                        .frame(egui::Frame::none().fill(win_bg).inner_margin(8.0))
                        .show(v_ctx, |ui| {
                            ui.horizontal(|ui| {
                                ui.heading(
                                    RichText::new(format!("Whisper: {}", w.peer))
                                        .color(win_accent),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("× Close").clicked() {
                                            close_win = true;
                                        }
                                        if ui.small_button("↙ Dock").clicked() {
                                            dock_back = true;
                                        }
                                    },
                                );
                            });
                            ui.separator();
                            let h = (ui.available_height() - 34.0).max(80.0);
                            egui::ScrollArea::vertical()
                                .id_salt(("whisper_det_scroll", &w.peer))
                                .max_height(h)
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    render_whisper_history_lines(
                                        ui,
                                        &w.history_lines,
                                        !w.lines.is_empty(),
                                        &self_name,
                                        &w.peer,
                                        &mut chat_action,
                                        &self.smiley_textures,
                                        &self.desctag_textures,
                                        &self.chat_img_textures,
                                        &self.player_badge_colors,
                                        &player_badges,
                                        &self_name_key,
                                        &win_colors,
                                    );
                                    for line in &w.lines {
                                        render_chat_line(
                                            ui,
                                            line,
                                            &mut chat_action,
                                            &self.smiley_textures,
                                            &self.desctag_textures,
                                            &self.chat_img_textures,
                                            &self.player_badge_colors,
                                            &player_badges,
                                            &self_name_key,
                                            &win_colors,
                                        );
                                    }
                                });
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let send_clicked = ui.button("Send").clicked();
                                    let input_w = (ui.available_width() - 10.0).max(60.0);
                                    let enter = ui
                                        .add(
                                            egui::TextEdit::singleline(&mut w.input)
                                                .hint_text(format!("Whisper {}...", w.peer))
                                                .desired_width(input_w),
                                        )
                                        .lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    if (send_clicked || enter) && !w.input.trim().is_empty() {
                                        send_msg = Some(w.input.trim().to_owned());
                                        w.input.clear();
                                    }
                                });
                            });
                        });
                }
            });

            if let Some(act) = chat_action {
                self.handle_chat_click_action(act);
            }
            if let Some(msg) = send_msg {
                if self.is_connected {
                    let cmd = if is_hyphenated_speech_furcadia_command(&msg) {
                        ClientCommand::Say(msg)
                    } else {
                        self.track_pending_whisper_echo(&peer, &msg);
                        ClientCommand::Whisper {
                            target: wire_player_name(&peer),
                            message: msg,
                        }
                    };
                    let _ = self.to_net.send(UiToNet::Send(cmd));
                }
            }
            if dock_back {
                if let Some(w) = self
                    .whispers
                    .iter_mut()
                    .find(|w| w.peer.eq_ignore_ascii_case(&peer))
                {
                    w.detached = false;
                }
                self.switch_chat_tab(Some(peer.clone()));
            } else if close_win {
                self.whispers
                    .retain(|w| !w.peer.eq_ignore_ascii_case(&peer));
            }
        }
    }

    fn render_sidebar(&mut self, ctx: &egui::Context) {
        let panel_bg = self.ui_panel();
        let accent = self.ui_accent();

        let sidebar_w = (272.0 + (self.portrait_size - 64.0).max(0.0)).min(380.0);
        egui::SidePanel::left("furc_sidebar")
            .exact_width(sidebar_w)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(panel_bg)
                    .stroke(Stroke::new(1.5_f32, GOLD_DIM))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("FURCADIA").size(18.0).strong().color(accent));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("About").on_hover_text("About & Credits").clicked() {
                            self.about_open = !self.about_open;
                        }
                        if ui.small_button("Help").on_hover_text("F1 Help").clicked() {
                            self.help_open = !self.help_open;
                        }
                        if ui
                            .small_button("Chars")
                            .on_hover_text("Open Character List in browser")
                            .clicked()
                        {
                            ctx.open_url(egui::OpenUrl::new_tab(
                                "https://cms.furcadia.com/character/list",
                            ));
                        }                      
                    });
                });

                ui.add_space(3.0);
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .small_button("+Char Window")
                        .on_hover_text("Launch a new Furcadia window for another character")
                        .clicked()
                    {
                        Self::launch_new_window();
                    }
                    if ui
                        .small_button("+Char Tab")
                        .on_hover_text("Keep this character logged in and log in another character in a tab")
                        .clicked()
                    {
                        self.suspend_active_session_to_bg(ctx);
                        self.active_char_tab_pos = self.bg_sessions.len();
                        self.screen = AppScreen::CharacterSelect;
                    }
                    let mention_label = if self.notify_mention {
                        "Mentions: ON"
                    } else {
                        "Mentions: OFF"
                    };
                    let mention_btn = egui::Button::new(
                        RichText::new(mention_label)
                            .font(FontId::new(12.0, self.ui_font_family(self.notify_mention)))
                            .strong()
                            .color(if self.notify_mention {
                                Color32::from_rgb(24, 16, 6)
                            } else {
                                Color32::from_rgb(195, 190, 210)
                            }),
                    )
                    .fill(if self.notify_mention {
                        accent
                    } else {
                        Color32::from_rgb(42, 30, 68)
                    })
                    .rounding(4.0);
                    let mention_hover = if self.mention_nicknames.trim().is_empty() {
                        "Toggle Main Chat mention alerts for your name & nicknames (configure nicknames in Settings -> General)".to_owned()
                    } else {
                        format!(
                            "Toggle Main Chat mention alerts (watching your name +: {}). Edit nicknames in Settings -> General.",
                            self.mention_nicknames.trim()
                        )
                    };
                    if ui.add(mention_btn).on_hover_text(mention_hover).clicked() {
                        self.notify_mention = !self.notify_mention;
                        self.save_config();
                    }
                });

                // Multi-character in-window session tabs (fixed order on click; drag to reorder)
                if !self.bg_sessions.is_empty() {
                    ui.add_space(3.0);
                    let total_tabs = self.bg_sessions.len() + 1;
                    let active_pos = self.active_char_tab_pos.min(self.bg_sessions.len());
                    let active_name = self.active_character_name();
                    let mut switch_to: Option<usize> = None;
                    let mut close_bg: Option<usize> = None;
                    let mut close_active = false;
                    let mut tab_rects: Vec<(usize, Rect)> = Vec::with_capacity(total_tabs);

                    let char_tab_sz = ((self.font_size as f32) - 1.0).clamp(11.0, 18.0).round();
                    let char_tab_regular = FontId::new(char_tab_sz, self.ui_font_family(false));
                    let char_tab_bold = FontId::new(char_tab_sz, self.ui_font_family(true));

                    ui.scope(|ui| {
                        ui.spacing_mut().button_padding = Vec2::new(6.0, 3.0);
                        ui.horizontal_wrapped(|ui| {
                            for vis in 0..total_tabs {
                                if vis == active_pos {
                                    let resp = ui.add(
                                        egui::Button::new(
                                            RichText::new(&active_name)
                                                .font(char_tab_bold.clone())
                                                .strong()
                                                .color(self.colors.tab_active_text),
                                        )
                                        .fill(self.colors.tab_active_bg)
                                        .rounding(4.0)
                                        .sense(Sense::click_and_drag()),
                                    );
                                    if resp.drag_started() {
                                        self.dragging_char_tab = Some(vis);
                                    }
                                    tab_rects.push((vis, resp.rect));
                                    if ui
                                        .small_button("×")
                                        .on_hover_text("Disconnect & close this character tab")
                                        .clicked()
                                    {
                                        close_active = true;
                                    }
                                } else {
                                    let bg_idx = if vis < active_pos { vis } else { vis - 1 };
                                    let bg = &self.bg_sessions[bg_idx];
                                    let lbl = if bg.unread_count > 0 {
                                        format!("{} ({})", bg.char_name, bg.unread_count)
                                    } else {
                                        bg.char_name.clone()
                                    };
                                    let resp = if bg.unread_count > 0 {
                                        ui.add(
                                            egui::Button::new(
                                                RichText::new(lbl)
                                                    .font(char_tab_bold.clone())
                                                    .strong()
                                                    .color(self.colors.tab_unread_text),
                                            )
                                            .fill(self.colors.tab_unread_bg)
                                            .rounding(4.0)
                                            .sense(Sense::click_and_drag()),
                                        )
                                    } else {
                                        ui.add(
                                            egui::Button::new(
                                                RichText::new(lbl)
                                                    .font(char_tab_regular.clone())
                                                    .color(self.colors.tab_inactive_text),
                                            )
                                            .fill(self.colors.tab_inactive_bg)
                                            .rounding(4.0)
                                            .sense(Sense::click_and_drag()),
                                        )
                                    };
                                    if resp.drag_started() {
                                        self.dragging_char_tab = Some(vis);
                                    } else if resp.clicked() {
                                        switch_to = Some(bg_idx);
                                    }
                                    tab_rects.push((vis, resp.rect));
                                    if ui
                                        .small_button("×")
                                        .on_hover_text("Disconnect & close this character tab")
                                        .clicked()
                                    {
                                        close_bg = Some(bg_idx);
                                    }
                                }
                            }
                        });
                    });

                    if !ui.input(|i| i.pointer.any_down()) {
                        self.dragging_char_tab = None;
                    } else if let (Some(from_vis), Some(pos)) =
                        (self.dragging_char_tab, ui.input(|i| i.pointer.interact_pos()))
                    {
                        if let Some(&(to_vis, _)) = tab_rects
                            .iter()
                            .find(|(v, r)| *v != from_vis && r.contains(pos))
                        {
                            self.swap_char_tabs(from_vis, to_vis);
                            self.dragging_char_tab = Some(to_vis);
                        }
                    }

                    if close_active {
                        self.close_active_character_tab(ctx);
                    } else if let Some(idx) = close_bg {
                        let sess = self.bg_sessions.remove(idx);
                        let _ = sess.to_net.send(UiToNet::Disconnect);
                        let removed_vis = if idx < active_pos { idx } else { idx + 1 };
                        if removed_vis < self.active_char_tab_pos {
                            self.active_char_tab_pos = self.active_char_tab_pos.saturating_sub(1);
                        }
                        self.active_char_tab_pos =
                            self.active_char_tab_pos.min(self.bg_sessions.len());
                    } else if let Some(idx) = switch_to {
                        self.switch_to_bg_session(idx, ctx);
                    }
                }

                ui.add_space(4.0);
                let self_port = self.self_portrait(ctx);
                egui::Frame::none()
                    .fill(self.ui_bg())
                    .stroke(Stroke::new(1.2_f32, GOLD_DIM))
                    .rounding(6.0)
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let port_frame = egui::Frame::none()
                                .fill(Color32::from_rgb(18, 12, 34))
                                .stroke(Stroke::new(1.5_f32, GOLD))
                                .rounding(4.0)
                                .inner_margin(2.0);
                            port_frame.show(ui, |ui| {
                                let psize = Vec2::splat(self.portrait_size);
                                if let Some(tex) = &self_port {
                                    ui.image((tex.id(), psize));
                                } else {
                                    ui.allocate_ui(psize, |ui| {
                                        ui.centered_and_justified(|ui| {
                                            ui.label(RichText::new("Portrait").size(11.0).color(GOLD_DIM));
                                        });
                                    });
                                }
                            });

                            ui.vertical(|ui| {
                                let char_name = self.active_character_name();
                                let status_color = if self.is_connected {
                                    Color32::from_rgb(90, 210, 120)
                                } else {
                                    Color32::from_rgb(200, 180, 90)
                                };
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new(&char_name).strong().color(accent));
                                    ui.colored_label(
                                        status_color,
                                        if self.is_connected { "• Online" } else { "• Offline" },
                                    );
                                });
                                let dream_label = self
                                    .current_dream_url
                                    .as_deref()
                                    .map(|u| u.trim_start_matches("furc://").trim_matches('/'))
                                    .filter(|s| !s.is_empty())
                                    .or(self.world.map_name.as_deref())
                                    .or(self.pending_dream_name.as_deref());
                                if let Some(dream) = dream_label {
                                    let dream_txt = if self.pending_dream_name.is_some() {
                                        match self.pending_dream_total_bytes.filter(|&t| t > 0) {
                                            Some(total) => {
                                                let pct = ((self.pending_dream_received_bytes as f64
                                                    / total as f64)
                                                    * 100.0)
                                                    .clamp(0.0, 100.0);
                                                format!("Dream: {dream} (DL {pct:.0}%)")
                                            }
                                            None if self.pending_dream_received_bytes > 0 => {
                                                format!(
                                                    "Dream: {dream} (DL {})",
                                                    format_bytes(self.pending_dream_received_bytes)
                                                )
                                            }
                                            None => format!("Dream: {dream} (Downloading...)"),
                                        }
                                    } else {
                                        format!("Dream: {dream}")
                                    };
                                    let info_sz = ((self.font_size as f32) - 1.0).clamp(11.0, 16.0).round();
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(dream_txt)
                                                .size(info_sz)
                                                .color(if self.pending_dream_name.is_some() {
                                                    GOLD
                                                } else {
                                                    Color32::LIGHT_GRAY
                                                }),
                                        )
                                        .truncate(),
                                    );
                                }
                                let info_sz = ((self.font_size as f32) - 1.0).clamp(11.0, 16.0).round();
                                if let Some((x, y)) = self.world.self_pos {
                                    ui.label(
                                        RichText::new(format!("Pos: ({x}, {y})"))
                                            .size(info_sz)
                                            .color(Color32::GRAY),
                                    );
                                }
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .selectable_label(self.afk_active, "AFK")
                                        .clicked()
                                    {
                                        self.afk_active = !self.afk_active;
                                        self.send_raw_cmd("afk");
                                    }
                                    if self.is_connected {
                                        if ui
                                            .small_button("Disconnect")
                                            .on_hover_text("Disconnect from server")
                                            .clicked()
                                        {
                                            let _ = self.to_net.send(UiToNet::Disconnect);
                                        }
                                    } else if ui.small_button("Reconn").clicked() {
                                        self.trigger_play();
                                    }
                                });
                                if ui.small_button("Change Portrait").clicked() {
                                    self.send_raw_cmd("portrchng");
                                    let self_wire = self.active_character_wire_name();
                                    if !self_wire.is_empty() && self.is_connected {
                                        self.silent_self_glook = true;
                                        let _ = self
                                            .to_net
                                            .send(UiToNet::Send(ClientCommand::Glook(self_wire)));
                                    }
                                }
                                {
                                    let mut txt = String::from("Cookies:");
                                    if let Some(c) = self.cookies_collected {
                                        txt.push_str(&format!(" {c} active"));
                                    }
                                    if let Some(t) = self.cookies_total {
                                        txt.push_str(&format!(" · {t} bank/inactive"));
                                    }
                                    if self.cookies_collected.is_none() && self.cookies_total.is_none() {
                                        txt = "Cookies (click to refresh)".to_owned();
                                    }
                                    if ui
                                        .add(
                                            egui::Label::new(
                                                RichText::new(txt)
                                                    .size(info_sz)
                                                    .color(Color32::from_rgb(235, 190, 120)),
                                            )
                                            .wrap()
                                            .sense(Sense::click()),
                                        )
                                        .on_hover_text("Click to query the server (`info`) for your current cookie counts")
                                        .clicked()
                                    {
                                        self.send_raw_cmd("info");
                                    }
                                }
                            });
                        });

                    });

                ui.add_space(6.0);
                self.render_looked_player_card(ui, ctx);
                let sidebar_tab_bold = egui::FontId::new(11.0, self.ui_font_family(true));
                let sidebar_tab_regular = egui::FontId::new(9.0, self.ui_font_family(false));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let tab_w = ((ui.available_width() - 5.0 * 4.0) / 6.0).floor().max(28.0);
                    for (tab, letter, sub, tip) in [
                        (SidebarTab::Move, "A", "MOVE", "Move & Actions"),
                        (SidebarTab::GoTo, "B", "GO TO", "Go To / Maps"),
                        (SidebarTab::Edit, "C", "EDIT", "Edit Character & Dream"),
                        (SidebarTab::Digo, "D", "DIGO", "Digo & Costumes"),
                        (SidebarTab::Emotes, "E", "DIGO", "Emotes & Digo #2"),
                        (SidebarTab::Misc, "F", "MISC", "Misc, Logs, Settings & Skin"),
                    ] {
                        let selected = self.sidebar_tab == tab;
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(tab_w, 34.0), Sense::click());
                        let fill = if selected {
                            accent
                        } else if resp.hovered() {
                            Color32::from_rgb(64, 46, 102)
                        } else {
                            Color32::from_rgb(42, 30, 68)
                        };
                        let border = if selected {
                            Color32::from_rgb(255, 232, 150)
                        } else if resp.hovered() {
                            accent
                        } else {
                            GOLD_DIM
                        };
                        let p = ui.painter();
                        p.rect_filled(rect, 5.0, fill);
                        p.rect_stroke(
                            rect,
                            5.0,
                            Stroke::new(if selected { 1.5_f32 } else { 1.0_f32 }, border),
                        );
                        p.text(
                            egui::pos2(rect.center().x, rect.top() + 10.5),
                            egui::Align2::CENTER_CENTER,
                            letter,
                            sidebar_tab_bold.clone(),
                            if selected { Color32::from_rgb(28, 18, 8) } else { accent },
                        );
                        p.text(
                            egui::pos2(rect.center().x, rect.bottom() - 9.0),
                            egui::Align2::CENTER_CENTER,
                            sub,
                            sidebar_tab_regular.clone(),
                            if selected {
                                Color32::from_rgb(28, 18, 8)
                            } else {
                                Color32::from_rgb(235, 230, 245)
                            },
                        );
                        if resp.on_hover_text(tip).clicked() {
                            self.sidebar_tab = tab;
                        }
                    }
                });
                ui.separator();

                let btn_fill = Color32::from_rgb(44, 32, 72);
                let btn_hover = Color32::from_rgb(64, 46, 102);
                let btn_active = Color32::from_rgb(80, 58, 122);
                {
                    let visuals = ui.visuals_mut();
                    visuals.widgets.inactive.weak_bg_fill = btn_fill;
                    visuals.widgets.inactive.bg_fill = btn_fill;
                    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, GOLD_DIM);
                    visuals.widgets.inactive.rounding = egui::Rounding::same(5.0);
                    visuals.widgets.hovered.weak_bg_fill = btn_hover;
                    visuals.widgets.hovered.bg_fill = btn_hover;
                    visuals.widgets.hovered.bg_stroke = Stroke::new(1.2_f32, accent);
                    visuals.widgets.hovered.rounding = egui::Rounding::same(5.0);
                    visuals.widgets.active.weak_bg_fill = btn_active;
                    visuals.widgets.active.bg_fill = btn_active;
                    visuals.widgets.active.bg_stroke = Stroke::new(1.5_f32, accent);
                    visuals.widgets.active.rounding = egui::Rounding::same(5.0);
                }

                egui::ScrollArea::vertical()
                    .id_salt("sidebar_tab_scroll")
                    .show(ui, |ui| match self.sidebar_tab {
                        SidebarTab::Move => {
                            ui.label(RichText::new("MOVE & POSTURE").strong().color(accent));
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    for (idx, lbl, cmd) in [
                                        (0u8, "Stand", posture_wire_command(0)),
                                        (1u8, "Sit", posture_wire_command(1)),
                                        (2u8, "Lie Down", posture_wire_command(2)),
                                    ] {
                                        let sel = self.posture == idx;
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new(lbl)
                                                        .strong()
                                                        .color(if sel {
                                                            Color32::from_rgb(24, 16, 6)
                                                        } else {
                                                            Color32::from_rgb(240, 236, 250)
                                                        }),
                                                )
                                                .fill(if sel { accent } else { btn_fill })
                                                .stroke(Stroke::new(
                                                    if sel { 1.5_f32 } else { 1.0_f32 },
                                                    if sel { Color32::from_rgb(255, 232, 150) } else { GOLD_DIM },
                                                ))
                                                .rounding(5.0)
                                                .min_size(Vec2::new(86.0, 28.0)),
                                            )
                                            .clicked()
                                        {
                                            self.posture = idx;
                                            self.send_raw_cmd(cmd);
                                        }
                                    }
                                });
                                egui::Frame::none()
                                    .fill(Color32::from_rgb(26, 18, 46))
                                    .stroke(Stroke::new(1.0_f32, GOLD_DIM))
                                    .rounding(6.0)
                                    .inner_margin(4.0)
                                    .show(ui, |ui| {
                                        ui.vertical(|ui| {
                                            ui.horizontal(|ui| {
                                                if ui
                                                    .add(egui::Button::new(RichText::new("7 ↖").strong()).min_size(Vec2::new(62.0, 36.0)))
                                                    .on_hover_text("Move NW (7 / Home)")
                                                    .clicked()
                                                    && self.is_connected
                                                {
                                                    let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(7)));
                                                }
                                                if ui
                                                    .add(egui::Button::new(RichText::new("9 ↗").strong()).min_size(Vec2::new(62.0, 36.0)))
                                                    .on_hover_text("Move NE (9 / PageUp)")
                                                    .clicked()
                                                    && self.is_connected
                                                {
                                                    let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(9)));
                                                }
                                            });
                                            ui.horizontal(|ui| {
                                                if ui
                                                    .add(egui::Button::new(RichText::new("1 ↙").strong()).min_size(Vec2::new(62.0, 36.0)))
                                                    .on_hover_text("Move SW (1 / End)")
                                                    .clicked()
                                                    && self.is_connected
                                                {
                                                    let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(1)));
                                                }
                                                if ui
                                                    .add(egui::Button::new(RichText::new("3 ↘").strong()).min_size(Vec2::new(62.0, 36.0)))
                                                    .on_hover_text("Move SE (3 / PageDown)")
                                                    .clicked()
                                                    && self.is_connected
                                                {
                                                    let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(3)));
                                                }
                                            });
                                        });
                                    });
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add(egui::Button::new("↺ Turn L").min_size(Vec2::new(112.0, 28.0)))
                                    .clicked()
                                {
                                    self.send_raw_cmd("<");
                                }
                                if ui
                                    .add(egui::Button::new("Turn R ↻").min_size(Vec2::new(112.0, 28.0)))
                                    .clicked()
                                {
                                    self.send_raw_cmd(">");
                                }
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new("Use (Ctrl-U)").strong().color(Color32::from_rgb(220, 250, 225)))
                                            .fill(Color32::from_rgb(36, 68, 52))
                                            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(90, 175, 120)))
                                            .min_size(Vec2::new(112.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    self.send_raw_cmd("use");
                                }
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new("Get (Ctrl-G)").strong().color(Color32::from_rgb(255, 240, 205)))
                                            .fill(Color32::from_rgb(72, 54, 34))
                                            .stroke(Stroke::new(1.0_f32, GOLD_DIM))
                                            .min_size(Vec2::new(112.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    self.send_raw_cmd("get");
                                }
                            });
                            ui.add_space(8.0);
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("EXIT").strong().color(Color32::WHITE))
                                        .fill(Color32::from_rgb(120, 40, 54))
                                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(210, 95, 110)))
                                        .min_size(Vec2::new(98.0, 28.0)),
                                )
                                .on_hover_text("Disconnect and return to Character Select")
                                .clicked()
                            {
                                let _ = self.to_net.send(UiToNet::Disconnect);
                                self.is_connected = false;
                                self.screen = AppScreen::CharacterSelect;
                            }
                        }
                        SidebarTab::GoTo => {
                            ui.label(RichText::new("GO TO / MAPS").strong().color(accent));
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                for (label, cmd) in [
                                    ("Vinca", "fdl furc://vinca/"),
                                    ("Imaginarium", "fdl furc://imaginarium/"),
                                    ("Naia", "fdl furc://naiagreen/"),
                                    ("Acropolis", "fdl furc://acropolis/"),
                                    ("Allegria", "fdl furc://allegria/"),
                                    ("Wylde", "fdl furc://wylde/"),
                                    ("Olde Towne", "fdl furc://oldetowne/"),
                                    ("Hawthorn", "fdl furc://hawthorn/"),
                                    ("FurN", "fdl furc://furn/"),
                                    ("Meovanni", "fdl furc://meovannivillage/"),
                                ] {
                                    if ui
                                        .add(
                                            egui::Button::new(label)
                                                .min_size(Vec2::new(108.0, 26.0)),
                                        )
                                        .clicked()
                                    {
                                        self.send_raw_cmd(cmd);
                                    }
                                }
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui.button("↩ BACK").clicked() {
                                    self.send_raw_cmd("goback");
                                }
                                if ui.button("WHO").clicked() && self.is_connected {
                                    let _ = self.to_net.send(UiToNet::Send(ClientCommand::Who));
                                }
                            });
                            ui.add_space(6.0);
                            ui.label(RichText::new("Player Join / Summon").color(accent));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.target_player_input)
                                    .hint_text("Player name")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.horizontal(|ui| {
                                let wire = wire_player_name(&self.target_player_input);
                                if ui.button("JOIN").clicked() && !wire.is_empty() {
                                    self.send_raw_cmd(&format!("join {wire}"));
                                }
                                if ui.button("SUMMON").clicked() && !wire.is_empty() {
                                    self.send_raw_cmd(&format!("summon {wire}"));
                                }
                                if ui.button("LOOK").clicked()
                                    && !wire.is_empty()
                                    && self.is_connected
                                {
                                    self.request_player_look(&wire);
                                }
                            });
                            ui.add_space(6.0);
                            ui.label(RichText::new("Dream URL").color(accent));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.dream_url_input)
                                    .desired_width(f32::INFINITY),
                            );
                            ui.horizontal(|ui| {
                                if ui.button("Enter Dream").clicked() {
                                    let url = self.dream_url_input.trim().to_owned();
                                    if !url.is_empty() {
                                        self.send_raw_cmd(&format!("fdl {url}"));
                                    }
                                }
                                if ui
                                    .button("+ Save Dream")
                                    .on_hover_text("Add this Dream URL to your Pounce Saved Dreams list")
                                    .clicked()
                                {
                                    let url = self.dream_url_input.trim().to_owned();
                                    if !url.is_empty() && !url.eq_ignore_ascii_case("furc://") {
                                        self.bookmark_dream(&url);
                                    } else if let Some(cur) = self
                                        .current_dream_url
                                        .clone()
                                        .or_else(|| self.world.map_name.clone())
                                    {
                                        self.bookmark_dream(&cur);
                                    }
                                }
                            });
                        }
                        SidebarTab::Edit => {
                            ui.label(RichText::new("EDIT").strong().color(accent));
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                if ui
                                    .add(egui::Button::new("Edit My Settings").min_size(Vec2::new(112.0, 28.0)))
                                    .clicked()
                                {
                                    self.show_settings_tab = true;
                                }
                                if ui
                                    .add(egui::Button::new("AFK Settings").min_size(Vec2::new(112.0, 28.0)))
                                    .clicked()
                                {
                                    self.afk_active = !self.afk_active;
                                    self.send_raw_cmd("afk");
                                }
                                if ui
                                    .add(egui::Button::new("Edit My Furre").min_size(Vec2::new(112.0, 28.0)))
                                    .clicked()
                                {
                                    ctx.open_url(egui::OpenUrl::new_tab("https://www.furcadia.com/fured"));
                                }
                                if ui
                                    .add(egui::Button::new("PLACE DREAM").min_size(Vec2::new(112.0, 28.0)))
                                    .clicked()
                                {
                                    self.send_raw_cmd("dream");
                                }
                            });
                            ui.add_space(6.0);
                            ui.label(RichText::new("Live Description").color(accent));
                            ui.add(
                                egui::TextEdit::multiline(&mut self.live_desc_input)
                                    .desired_rows(4)
                                    .desired_width(f32::INFINITY),
                            );
                            if ui.button("Update Description").clicked() && self.is_connected {
                                let clean_desc = self
                                    .live_desc_input
                                    .trim()
                                    .replace(['\r', '\n'], " ");
                                let _ = self.to_net.send(UiToNet::Send(ClientCommand::Desc(
                                    clean_desc,
                                )));
                            }
                            ui.add_space(6.0);
                            if let Some(map) = &self.world.map {
                                ui.label(
                                    RichText::new(format!(
                                        "Loaded Map: {}x{}",
                                        map.width, map.height
                                    ))
                                    .size(12.0)
                                    .color(Color32::LIGHT_GRAY),
                                );
                            }
                        }
                        SidebarTab::Digo => {
                            ui.label(RichText::new("DIGO TOGGLES & WINGS").strong().color(accent));
                            if let Some(ch) = self.selected_character() {
                                ui.label(
                                    RichText::new(format!("Colors: {}", ch.colors))
                                        .size(11.0)
                                        .color(Color32::LIGHT_GRAY),
                                );
                            }
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("Cycle Wings").on_hover_text("Cycle owned wings (`wings)").clicked() {
                                    self.send_raw_cmd("wings");
                                }
                                if ui.button("Desctags").on_hover_text("List/toggle desctags (`desctags)").clicked() {
                                    self.send_raw_cmd("desctags");
                                }
                                if ui.button("Digo Market").on_hover_text("Open Digo Market in browser").clicked() {
                                    ctx.open_url(egui::OpenUrl::new_tab(
                                        "https://www.furcadia.com/digo/",
                                    ));
                                }
                            });

                            ui.add_space(6.0);
                            ui.label(RichText::new("Wings (Toggle On/Off)").size(12.0).color(accent));
                            ui.horizontal_wrapped(|ui| {
                                for (label, cmd) in [
                                    ("Classic", "clsscwngs"),
                                    ("Tricolor", "triwngs"),
                                    ("Butterfly", "bflywngs"),
                                    ("Bat", "batwngs"),
                                    ("Prime", "primewngs"),
                                    ("Dragonfly", "dfwings"),
                                ] {
                                    if ui
                                        .add(egui::Button::new(label).min_size(Vec2::new(68.0, 24.0)))
                                        .on_hover_text(format!("Toggle {label} wings (`{cmd})"))
                                        .clicked()
                                    {
                                        self.send_raw_cmd(cmd);
                                    }
                                }
                            });

                            ui.add_space(6.0);
                            ui.label(RichText::new("Mythical & Special Digos (Toggle)").size(12.0).color(accent));
                            ui.horizontal_wrapped(|ui| {
                                for (label, cmd) in [
                                    ("Dragon", "dragon"),
                                    ("Phoenix", "phoenix"),
                                    ("Gryffe", "gryffe"),
                                    ("Unicorn", "unicorn"),
                                    ("Naga", "naga"),
                                    ("Kirin", "kirin"),
                                    ("Gargoyle", "gargoyle"),
                                    ("Angelcat", "angelcat"),
                                    ("Purrwing", "purrwing"),
                                    ("Kitter Sm", "kitw1"),
                                    ("Kitter Lg", "kitw2"),
                                    ("Kiwi", "kiwi"),
                                    ("Werewolf", "werewolf"),
                                    ("Furling", "furling"),
                                    ("Noble (Cycle)", "noble"),
                                ] {
                                    if ui
                                        .add(egui::Button::new(label).min_size(Vec2::new(72.0, 24.0)))
                                        .on_hover_text(format!("Toggle {label} (`{cmd})"))
                                        .clicked()
                                    {
                                        self.send_raw_cmd(cmd);
                                    }
                                }
                            });

                            ui.add_space(6.0);
                            ui.label(RichText::new("Ferians (Toggle)").size(12.0).color(accent));
                            ui.horizontal_wrapped(|ui| {
                                for (label, cmd) in [
                                    ("Wolven", "wolven"),
                                    ("Foxen", "foxen"),
                                    ("Catten", "catten"),
                                    ("Owlen", "owlen"),
                                    ("Ferian Dragon", "mfdragon"),
                                    ("Ferian Unicorn", "mfunicorn"),
                                ] {
                                    if ui
                                        .add(egui::Button::new(label).min_size(Vec2::new(72.0, 24.0)))
                                        .on_hover_text(format!("Toggle {label} (`{cmd})"))
                                        .clicked()
                                    {
                                        self.send_raw_cmd(cmd);
                                    }
                                }
                            });

                            ui.add_space(6.0);
                            ui.label(RichText::new("Nobles (Toggle)").size(12.0).color(accent));
                            ui.horizontal_wrapped(|ui| {
                                for (label, cmd) in [
                                    ("Canine", "noblecanine"),
                                    ("Feline", "noblefeline"),
                                    ("Equine", "nobleequine"),
                                    ("Lapine", "noblelapine"),
                                    ("Musteline", "noblemusteline"),
                                    ("Rodent", "noblerodent"),
                                    ("Squirrel", "noblesquirrel"),
                                    ("Hyooman", "noblehyooman"),
                                ] {
                                    if ui
                                        .add(egui::Button::new(label).min_size(Vec2::new(72.0, 24.0)))
                                        .on_hover_text(format!("Toggle Noble {label} (`{cmd})"))
                                        .clicked()
                                    {
                                        self.send_raw_cmd(cmd);
                                    }
                                }
                            });
                        }
                        SidebarTab::Emotes => {
                            ui.label(RichText::new("DIGO ABILITIES & ACTIONS").strong().color(accent));
                            ui.add_space(2.0);
                            ui.label(RichText::new("Digo Magic & Effects").size(12.0).color(accent));
                            ui.horizontal_wrapped(|ui| {
                                for (label, cmd) in [
                                    ("Magic", "magic"),
                                    ("Fly", "fly"),
                                    ("Flame", "flame"),
                                    ("Breath", "breath"),
                                    ("Glamour", "glamour"),
                                    ("Splash", "splash"),
                                    ("Stone", "stone"),
                                    ("Kirin Cloud", "kirincloud"),
                                    ("Purify", "purify"),
                                ] {
                                    if ui
                                        .add(egui::Button::new(label).min_size(Vec2::new(72.0, 24.0)))
                                        .on_hover_text(format!("Send `{cmd}"))
                                        .clicked()
                                    {
                                        self.send_raw_cmd(cmd);
                                    }
                                }
                            });
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new("Size:").size(12.0).color(Color32::LIGHT_GRAY));
                                for (label, cmd) in [
                                    ("50%", "kittersize 50"),
                                    ("100%", "kittersize 100"),
                                    ("150%", "kittersize 150"),
                                ] {
                                    if ui
                                        .small_button(label)
                                        .on_hover_text(format!("Send `{cmd}"))
                                        .clicked()
                                    {
                                        self.send_raw_cmd(cmd);
                                    }
                                }
                            });

                            ui.add_space(6.0);
                            ui.label(RichText::new("Social & Actions").size(12.0).color(accent));
                            ui.horizontal_wrapped(|ui| {
                                for (label, cmd) in [
                                    ("Lead", "lead"),
                                    ("Follow", "follow"),
                                    ("Stop", "stop"),
                                    ("Get", "get"),
                                    ("Use", "use"),
                                    ("Info", "info"),
                                    ("Summon", "summon"),
                                    ("Join", "join"),
                                    ("Decline", "decline"),
                                    ("Cookie", "buy-cookie"),
                                ] {
                                    if ui
                                        .add(egui::Button::new(label).min_size(Vec2::new(68.0, 24.0)))
                                        .clicked()
                                    {
                                        self.send_raw_cmd(cmd);
                                    }
                                }
                            });

                            ui.add_space(6.0);
                            ui.label(RichText::new("Emotes").size(12.0).color(accent));
                            ui.horizontal_wrapped(|ui| {
                                for (label, emote) in [
                                    ("Wave", ":waves."),
                                    ("Smile", ":smiles."),
                                    ("Nod", ":nods."),
                                    ("Laugh", ":laughs!"),
                                    ("Bow", ":bows."),
                                    ("Cheer", ":cheers!"),
                                    ("Dance", ":dances!"),
                                    ("Purr", ":purrs."),
                                    ("Howl", ":howls!"),
                                    ("Hug", ":hugs!"),
                                    ("Blush", ":blushes."),
                                    ("Wink", ":winks."),
                                ] {
                                    if ui
                                        .add(egui::Button::new(label).min_size(Vec2::new(68.0, 24.0)))
                                        .clicked()
                                    {
                                        self.send_raw_cmd(emote);
                                    }
                                }
                            });
                        }
                        SidebarTab::Misc => {
                            ui.label(RichText::new("MISC & TOOLS").strong().color(accent));
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                if ui
                                    .add(egui::Button::new("View Log").min_size(Vec2::new(112.0, 26.0)))
                                    .clicked()
                                {
                                    self.open_log_viewer();
                                }
                                if self.tray.is_active()
                                    && ui
                                        .add(
                                            egui::Button::new("Hide to Tray")
                                                .min_size(Vec2::new(112.0, 26.0)),
                                        )
                                        .clicked()
                                {
                                    self.hide_to_tray(ctx);
                                }
                                if ui
                                    .add(egui::Button::new("Forums").min_size(Vec2::new(112.0, 26.0)))
                                    .clicked()
                                {
                                    ctx.open_url(egui::OpenUrl::new_tab("https://forums.furcadia.com/"));
                                }
                                if ui
                                    .add(egui::Button::new("News").min_size(Vec2::new(112.0, 26.0)))
                                    .clicked()
                                {
                                    ctx.open_url(egui::OpenUrl::new_tab("https://www.furcadia.com/news/"));
                                }
                                if ui
                                    .add(egui::Button::new("Digo Market").min_size(Vec2::new(112.0, 26.0)))
                                    .clicked()
                                {
                                    ctx.open_url(egui::OpenUrl::new_tab("https://www.furcadia.com/digo/"));
                                }
                                if ui
                                    .add(egui::Button::new("Screenshot").min_size(Vec2::new(112.0, 26.0)))
                                    .clicked()
                                {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
                                    self.push_system_line("emit", "Screenshot requested.");
                                }
                                if ui
                                    .add(
                                        egui::Button::new(if self.sound_enabled {
                                            "Sound: ON"
                                        } else {
                                            "Sound: OFF"
                                        })
                                        .min_size(Vec2::new(112.0, 26.0)),
                                    )
                                    .clicked()
                                {
                                    self.sound_enabled = !self.sound_enabled;
                                    self.save_config();
                                    if self.sound_enabled {
                                        self.play_sound("online.wav");
                                    }
                                }
                                if ui
                                    .add(
                                        egui::Button::new(if self.music_enabled {
                                            "Music: ON"
                                        } else {
                                            "Music: OFF"
                                        })
                                        .min_size(Vec2::new(112.0, 26.0)),
                                    )
                                    .clicked()
                                {
                                    self.music_enabled = !self.music_enabled;
                                    self.save_config();
                                    if !self.music_enabled {
                                        self.stop_music();
                                    } else if let Some(dir) = self.current_dream_dir.clone() {
                                        self.play_dream_music(&dir);
                                    }
                                }
                            });

                            ui.add_space(6.0);
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("Settings & Windows Importer").strong())
                                        .min_size(Vec2::new(ui.available_width(), 28.0)),
                                )
                                .clicked()
                            {
                                self.show_settings_tab = true;
                            }

                            ui.add_space(8.0);
                            ui.label(RichText::new("Animations (F11)").strong().color(accent));
                            ui.horizontal(|ui| {
                                for mode in ["Full", "Half", "Off"] {
                                    if ui.radio(self.anim_mode == mode, mode).clicked() {
                                        self.anim_mode = mode.to_owned();
                                        self.save_config();
                                    }
                                }
                            });

                            ui.add_space(8.0);
                            ui.label(RichText::new("POUNCE & WINDOWS").strong().color(accent));
                            ui.horizontal(|ui| {
                                if ui
                                    .button(if self.pounce_visible {
                                        "Hide Pounce"
                                    } else {
                                        "Show Pounce"
                                    })
                                    .clicked()
                                {
                                    self.pounce_visible = !self.pounce_visible;
                                    self.save_config();
                                }
                                if ui
                                    .button(if self.pounce_detached {
                                        "Dock Pounce"
                                    } else {
                                        "Detach Pounce"
                                    })
                                    .clicked()
                                {
                                    self.pounce_detached = !self.pounce_detached;
                                    self.save_config();
                                }
                            });
                            ui.add_space(8.0);
                            ui.label(RichText::new("Dream Window Skin").strong().color(accent));
                            for t in [
                                DreamTheme::ClassicPurple,
                                DreamTheme::DragonHoard,
                                DreamTheme::KitterMagic,
                                DreamTheme::FerianWylde,
                            ] {
                                if ui.radio(!self.colors.custom_ui_bg && self.theme == t, t.label()).clicked() {
                                    self.theme = t;
                                    self.colors.custom_ui_bg = false;
                                    self.save_config();
                                }
                            }
                            ui.add_space(6.0);
                            if ui
                                .checkbox(&mut self.show_debug_packets, "Log raw/extended packets")
                                .changed()
                            {
                                self.save_config();
                            }
                        }
                    });
            });
    }

    fn render_help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.help_open;
        let (_, _, accent) = self.effective_colors();
        egui::Window::new("Furcadia Help & Hotkeys (F1)")
            .open(&mut open)
            .resizable(true)
            .default_size(Vec2::new(500.0, 400.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Quick Reference").strong().color(accent));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Ask Live Beekin Help (`help)").clicked() {
                            self.send_raw_cmd("help");
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (cat, items) in [
                        (
                            "Movement & Posture",
                            "Arrows / Num 1,3,7,9: Move  |  Shift+Left/Right or < / >: Rotate\nCtrl+L / `liedown / `sit / `stand: Posture  |  F2 / Ctrl+G / `get: Pick up/Drop\nF3 / Ctrl+U: Use held item  |  Tab: Toggle avatar names",
                        ),
                        (
                            "Chat & Formatting",
                            ":action: Emote  |  /\"name\" msg: Whisper  |  /%%name msg: Offline msg\nShift+Ctrl+B/I/U: Bold/Italic/Underline  |  Ctrl+1..0 / Shift+Ctrl+1..5: Emoticons\nCtrl+I/J/K: Recall last sent / whisper target / received whisper\nF5 / F6: Scroll chat up/down  |  F8 / Ctrl+R: Open last posted link  |  Esc: Clear input",
                        ),
                        (
                            "Audio, Views & System",
                            "Ctrl+P: Toggle sound  |  Ctrl+O: Toggle music  |  Ctrl+Y: Test/re-init sound\nCtrl+N: Reload dream patches  |  F11: Cycle animation speed (Full/Half/Off)\nCtrl+D: Switch chat/whisper tab  |  Ctrl+W: Close active tab  |  Ctrl+F: Font picker  |  F4: Who",
                        ),
                    ] {
                        ui.label(RichText::new(cat).strong().color(accent));
                        ui.label(RichText::new(items).size(12.0).color(Color32::LIGHT_GRAY));
                        ui.add_space(6.0);
                    }
                });
            });
        self.help_open = open;
    }

    fn render_about_window(&mut self, ctx: &egui::Context) {
        let mut open = self.about_open;
        let (_, _, accent) = self.effective_colors();
        egui::Window::new("About Furcadia Native Client")
            .open(&mut open)
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.heading(RichText::new("Furcadia Native Client").color(accent));
                    ui.label(
                        RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                            .size(13.0)
                            .color(Color32::LIGHT_GRAY),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !self.update_checking,
                                egui::Button::new("Check for Updates"),
                            )
                            .clicked()
                        {
                            self.trigger_update_check(ctx);
                        }
                        if let Some(status) = &self.update_status {
                            ui.label(RichText::new(status).size(12.0).color(Color32::LIGHT_GRAY));
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Native cross-platform Furcadia client for Linux and Windows.")
                            .size(12.0),
                    );
                });

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);

                ui.label(RichText::new("Open Source Resources & Credits").strong().color(accent));
                ui.add_space(4.0);
                for (name, desc) in [
                    ("eframe / egui (MIT / Apache-2.0)", "Immediate-mode Rust GUI framework"),
                    ("image (MIT / Apache-2.0)", "PNG and image decoding"),
                    ("lzma-rs (MIT)", "LZMA decompression for Fox5/FR01 archives"),
                    ("Rust Standard Library (MIT / Apache-2.0)", "Networking, filesystem, and concurrency"),
                    ("Furcadia", "Protocol & game world by Dragon's Eye Productions, Inc. Uses user-supplied local Furcadia assets."),
                ] {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(name).strong().size(12.0).color(Color32::WHITE));
                        ui.label(RichText::new(format!("- {desc}")).size(12.0).color(Color32::LIGHT_GRAY));
                    });
                }

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                ui.vertical_centered(|ui| {
                    if let Some(tex) = &self.bear_logo {
                        ui.add(
                            egui::Image::new(tex)
                                .max_size(Vec2::splat(96.0))
                                .rounding(8.0),
                        );
                    }
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Bear Built  •  CC BY-NC-SA 4.0 (Non-Commercial)")
                            .size(12.0)
                            .color(Color32::GRAY),
                    );
                });
            });
        self.about_open = open;
    }

    fn handle_in_game_hotkeys(&mut self, ctx: &egui::Context) {
        let mut raw_cmd: Option<&'static str> = None;
        let mut send_who = false;
        let mut toggle_names = false;
        let mut toggle_sound = false;
        let mut toggle_music = false;
        let mut test_sound = false;
        let mut reload_patches = false;
        let mut open_last_link = false;
        let mut cycle_anim = false;
        let mut cycle_posture = false;
        let mut switch_chat_tab = false;
        let mut close_active_tab = false;
        let mut toggle_font = false;
        let mut toggle_help = false;
        let mut clear_input = false;
        let mut insert_tag: Option<&'static str> = None;
        let mut recall_mode: u8 = 0; // 1=sent, 2=wh_target, 3=wh_recv
        let mut move_dir: Option<u8> = None;
        let wants_text = ctx.wants_keyboard_input();
        let single_line_input = !self.chat_input.contains('\n');
        let empty_input = self.chat_input.is_empty();

        ctx.input_mut(|i| {
            let ctrl = i.modifiers.ctrl || i.modifiers.command;
            let shift = i.modifiers.shift;
            if i.key_pressed(egui::Key::Escape) {
                clear_input = true;
            } else if i.key_pressed(egui::Key::F1) || (!wants_text && ctrl && i.key_pressed(egui::Key::Z)) {
                toggle_help = true;
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Tab) {
                toggle_names = true;
            } else if i.key_pressed(egui::Key::F2) || (ctrl && i.key_pressed(egui::Key::G)) {
                raw_cmd = Some("get");
            } else if i.key_pressed(egui::Key::F3) || (ctrl && i.key_pressed(egui::Key::U)) {
                raw_cmd = Some("use");
            } else if i.key_pressed(egui::Key::F4) {
                send_who = true;
            } else if ctrl && !shift && i.key_pressed(egui::Key::W) {
                close_active_tab = true;
            } else if i.key_pressed(egui::Key::F5) {
                self.chat_scroll_delta += 140.0;
            } else if i.key_pressed(egui::Key::F6) {
                self.chat_scroll_delta -= 140.0;
            } else if i.key_pressed(egui::Key::F8) || (ctrl && i.key_pressed(egui::Key::R)) {
                open_last_link = true;
            } else if i.key_pressed(egui::Key::F11) {
                cycle_anim = true;
            } else if ctrl && i.key_pressed(egui::Key::P) {
                toggle_sound = true;
            } else if ctrl && i.key_pressed(egui::Key::O) {
                toggle_music = true;
            } else if ctrl && i.key_pressed(egui::Key::Y) {
                test_sound = true;
            } else if ctrl && i.key_pressed(egui::Key::N) {
                reload_patches = true;
            } else if ctrl && i.key_pressed(egui::Key::L) {
                cycle_posture = true;
            } else if ctrl && i.key_pressed(egui::Key::D) {
                switch_chat_tab = true;
            } else if ctrl && i.key_pressed(egui::Key::F) {
                toggle_font = true;
            } else if ctrl && shift && i.key_pressed(egui::Key::B) {
                insert_tag = Some("<b></b>");
            } else if ctrl && shift && i.key_pressed(egui::Key::I) {
                insert_tag = Some("<i></i>");
            } else if ctrl && shift && i.key_pressed(egui::Key::U) {
                insert_tag = Some("<u></u>");
            } else if ctrl && !shift && i.key_pressed(egui::Key::I) {
                recall_mode = 1;
            } else if ctrl && !shift && i.key_pressed(egui::Key::J) {
                recall_mode = 2;
            } else if ctrl && !shift && i.key_pressed(egui::Key::K) {
                recall_mode = 3;
            } else if ctrl && !shift && i.key_pressed(egui::Key::B) {
                raw_cmd = Some("goback");
            } else if ctrl && shift && i.key_pressed(egui::Key::Num1) {
                insert_tag = Some("#SK");
            } else if ctrl && shift && i.key_pressed(egui::Key::Num2) {
                insert_tag = Some("#SL");
            } else if ctrl && shift && i.key_pressed(egui::Key::Num3) {
                insert_tag = Some("#SM");
            } else if ctrl && shift && i.key_pressed(egui::Key::Num4) {
                insert_tag = Some("#SN");
            } else if ctrl && shift && i.key_pressed(egui::Key::Num5) {
                insert_tag = Some("#SO");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num1) {
                insert_tag = Some("#SA");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num2) {
                insert_tag = Some("#SB");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num3) {
                insert_tag = Some("#SC");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num4) {
                insert_tag = Some("#SD");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num5) {
                insert_tag = Some("#SE");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num6) {
                insert_tag = Some("#SF");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num7) {
                insert_tag = Some("#SG");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num8) {
                insert_tag = Some("#SH");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num9) {
                insert_tag = Some("#SI");
            } else if ctrl && !shift && i.key_pressed(egui::Key::Num0) {
                insert_tag = Some("#SJ");
            } else if (shift && i.key_pressed(egui::Key::ArrowLeft))
                || (!wants_text && i.key_pressed(egui::Key::Insert))
                || (ctrl && i.key_pressed(egui::Key::ArrowLeft))
            {
                raw_cmd = Some("<");
            } else if (shift && i.key_pressed(egui::Key::ArrowRight))
                || (!wants_text && i.key_pressed(egui::Key::Delete))
                || (ctrl && i.key_pressed(egui::Key::ArrowRight))
            {
                raw_cmd = Some(">");
            }
            // Let the focused text editor handle arrows for caret navigation. The chat input
            // is also a TextEdit, so this intentionally avoids sending movement while typing
            // in chat or whisper fields.
            if self.is_connected && move_dir.is_none() && !wants_text {
                if !shift && i.consume_key(egui::Modifiers::NONE, egui::Key::Home) {
                    move_dir = Some(7);
                } else if !shift && i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp) {
                    move_dir = Some(9);
                } else if !shift && i.consume_key(egui::Modifiers::NONE, egui::Key::End) {
                    move_dir = Some(1);
                } else if !shift && i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown) {
                    move_dir = Some(3);
                } else if !shift && !ctrl && single_line_input && i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                    move_dir = Some(9);
                } else if !shift && !ctrl && single_line_input && i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                    move_dir = Some(1);
                } else if !shift && !ctrl && empty_input && i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                    move_dir = Some(7);
                } else if !shift && !ctrl && empty_input && i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                    move_dir = Some(3);
                }
            }
        });

        if let Some(dir) = move_dir {
            let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(dir)));
        }

        if clear_input {
            self.chat_input.clear();
        }
        if toggle_help {
            self.help_open = !self.help_open;
        }
        if let Some(tag) = insert_tag {
            self.chat_input.push_str(tag);
        }
        match recall_mode {
            1 if !self.last_sent_msg.is_empty() => self.chat_input = self.last_sent_msg.clone(),
            2 if !self.last_whisper_target.is_empty() => {
                self.chat_input = format!("/\"{}\" ", self.last_whisper_target);
            }
            3 if !self.last_received_whisper_from.is_empty() => {
                self.chat_input = format!("/\"{}\" ", self.last_received_whisper_from);
            }
            _ => {}
        }
        if toggle_names {
            self.show_avatar_names = !self.show_avatar_names;
            self.save_config();
        }
        if toggle_sound {
            self.sound_enabled = !self.sound_enabled;
            self.save_config();
        }
        if toggle_music {
            self.music_enabled = !self.music_enabled;
            self.save_config();
            if !self.music_enabled {
                self.stop_music();
            } else if let Some(dir) = self.current_dream_dir.clone() {
                self.play_dream_music(&dir);
            }
        }
        if test_sound {
            self.sound_enabled = true;
            self.play_sound("online.wav");
            self.push_system_line("emit", "Sound system initialized.");
        }
        if reload_patches {
            self.pending_stock_patch_downloads.clear();
            if let Some(dir) = self.current_dream_dir.clone() {
                self.load_dream_dir(&dir);
                self.push_system_line("emit", "Reloaded dream patches.");
            }
            self.ensure_stock_patches_for_current_map();
        }
        if open_last_link {
            let last_url = self.chat_log.iter().rev().find_map(|line| {
                line.spans.iter().rev().find_map(|s| match s {
                    furcadia_client::ChatSpan::Text { style, .. } => style.link.clone(),
                    _ => None,
                })
            });
            if let Some(url) = last_url {
                if url.starts_with("furc://") {
                    self.send_raw_cmd(&format!("fdl {url}"));
                } else {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
            }
        }
        if cycle_anim {
            self.anim_mode = match self.anim_mode.as_str() {
                "Full" => "Half".to_owned(),
                "Half" => "Off".to_owned(),
                _ => "Full".to_owned(),
            };
            self.save_config();
            self.push_system_line("emit", &format!("Animations set to: {}", self.anim_mode));
        }
        if cycle_posture {
            self.posture = (self.posture + 1) % 3;
            let cmd = posture_wire_command(self.posture);
            self.send_raw_cmd(cmd);
        }
        if switch_chat_tab {
            if self.active_chat_tab.is_some() {
                self.switch_chat_tab(None);
            } else if let Some(w) = self.whispers.iter().find(|w| !w.detached) {
                self.switch_chat_tab(Some(w.peer.clone()));
            }
        }
        if close_active_tab {
            if let Some(peer) = self.active_chat_tab.clone() {
                self.close_whisper_tab(&peer);
            }
        }
        if toggle_font {
            self.font_popup_open = !self.font_popup_open;
        }
        if let Some(cmd) = raw_cmd {
            self.send_raw_cmd(cmd);
        }
        if send_who && self.is_connected {
            let _ = self.to_net.send(UiToNet::Send(ClientCommand::Who));
        }
    }

    fn close_whisper_tab(&mut self, peer: &str) {
        let was_active = self
            .active_chat_tab
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(peer));
        let docked_peers: Vec<String> = self
            .whispers
            .iter()
            .filter(|w| !w.detached)
            .map(|w| w.peer.clone())
            .collect();
        let closed_docked_idx = docked_peers
            .iter()
            .position(|p| p.eq_ignore_ascii_case(peer));

        self.whispers
            .retain(|w| !w.peer.eq_ignore_ascii_case(peer));

        if was_active {
            let remaining_docked: Vec<String> = self
                .whispers
                .iter()
                .filter(|w| !w.detached)
                .map(|w| w.peer.clone())
                .collect();
            let next_tab = closed_docked_idx.and_then(|idx| {
                remaining_docked
                    .get(idx)
                    .or_else(|| idx.checked_sub(1).and_then(|prev| remaining_docked.get(prev)))
                    .cloned()
            });
            if let Some(ref next_peer) = next_tab {
                if let Some(w) = self
                    .whispers
                    .iter_mut()
                    .find(|w| w.peer.eq_ignore_ascii_case(next_peer))
                {
                    w.unread = 0;
                }
            } else {
                self.main_chat_unread = 0;
            }
            self.switch_chat_tab(next_tab);
            self.focus_chat_input = true;
        }
    }

    fn render_in_game_screen(&mut self, ctx: &egui::Context) {
        let mut closed_settings_this_frame = false;
        if self.show_settings_tab
            && ctx.input(|i| {
                (i.modifiers.ctrl || i.modifiers.command)
                    && !i.modifiers.shift
                    && i.key_pressed(egui::Key::W)
            })
        {
            self.show_settings_tab = false;
            closed_settings_this_frame = true;
        }
        if self.show_settings_tab {
            self.render_detached_viewports(ctx);
            self.render_sidebar(ctx);
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(self.ui_bg()).inner_margin(16.0))
                .show(ctx, |ui| {
                    if ui.button("← Back to Game").clicked() {
                        self.show_settings_tab = false;
                    }
                    ui.add_space(8.0);
                    self.render_settings_tab(ui, ctx);
                });
            return;
        }
        if !closed_settings_this_frame {
            self.handle_in_game_hotkeys(ctx);
        }
        self.render_detached_viewports(ctx);
        self.render_sidebar(ctx);

        if self.pounce_visible && !self.pounce_detached {
            let pounce_min_w = 220.0_f32;
            let pounce_max_w = (ctx.screen_rect().width() * 0.35).max(240.0);
            let pounce_w = self.pounce_panel_width.clamp(pounce_min_w, pounce_max_w);
            egui::SidePanel::right("pounce_panel")
                .exact_width(pounce_w)
                .resizable(false)
                .frame(
                    egui::Frame::none()
                        .fill(self.ui_panel())
                        .stroke(Stroke::new(1.5_f32, GOLD_DIM))
                        .inner_margin(8.0),
                )
                .show(ctx, |ui| {
                    self.render_pounce_contents(ui, false);
                });
            let accent = self.ui_accent();
            egui::SidePanel::right("pounce_resize_handle")
                .exact_width(5.0)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    let resp = ui.allocate_response(ui.available_size(), Sense::drag());
                    if resp.hovered() || resp.dragged() {
                        ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        let x = resp.rect.center().x;
                        ui.painter()
                            .vline(x, resp.rect.y_range(), Stroke::new(1.5_f32, accent));
                    }
                    if resp.dragged() && ui.input(|i| i.pointer.primary_down()) {
                        self.pounce_panel_width =
                            (pounce_w - resp.drag_delta().x).clamp(pounce_min_w, pounce_max_w);
                    }
                });
        }

        let (eff_bg, eff_panel, accent) = self.effective_colors();
        egui::TopBottomPanel::top("map_options_bar")
            .frame(
                egui::Frame::none()
                    .fill(eff_panel)
                    .stroke(Stroke::new(1.0_f32, GOLD_DIM))
                    .inner_margin(egui::Margin::symmetric(8.0, 4.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("↖").on_hover_text("Move NW (7)").clicked() && self.is_connected {
                        let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(7)));
                    }
                    if ui.small_button("↗").on_hover_text("Move NE (9)").clicked() && self.is_connected {
                        let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(9)));
                    }
                    if ui.small_button("↙").on_hover_text("Move SW (1)").clicked() && self.is_connected {
                        let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(1)));
                    }
                    if ui.small_button("↘").on_hover_text("Move SE (3)").clicked() && self.is_connected {
                        let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(3)));
                    }
                    if ui.small_button("Center").clicked() {
                        self.cam_offset = Vec2::ZERO;
                    }
                    if ui
                        .selectable_label(self.show_avatar_names, "Names")
                        .on_hover_text("Toggle names (Tab)")
                        .clicked()
                    {
                        self.show_avatar_names = !self.show_avatar_names;
                        self.save_config();
                    }
                    ui.label(
                        RichText::new(format!("{} Visible", self.last_visible_avatar_count))
                            .size(11.0)
                            .color(Color32::LIGHT_GRAY),
                    )
                    .on_hover_text("Visible avatars on the map");
                    ui.separator();
                    if ui.small_button("Who").on_hover_text("Who is online (F4)").clicked() && self.is_connected {
                        let _ = self.to_net.send(UiToNet::Send(ClientCommand::Who));
                    }
                    let anim_label = format!("Anim: {}", self.anim_mode);
                    if ui.small_button(anim_label).on_hover_text("Cycle animation speed (Full / Half / Off)").clicked() {
                        self.anim_mode = match self.anim_mode.as_str() {
                            "Full" => "Half".to_owned(),
                            "Half" => "Off".to_owned(),
                            _ => "Full".to_owned(),
                        };
                        self.save_config();
                    }
                    if ui.small_button("Settings").on_hover_text("Open Settings").clicked() {
                        self.show_settings_tab = !self.show_settings_tab;
                    }
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let map_size_label = "Map Size";
                            if ui
                                .selectable_label(self.limit_view_range, map_size_label)
                                .on_hover_text(
                                    "Switch between Furcadia's 1× 624×352 default map and the expanded map",
                                )
                                .clicked()
                            {
                                self.limit_view_range = !self.limit_view_range;
                                self.zoom = if self.limit_view_range { 1.0 } else { 1.25 };
                                self.save_config();
                            }
                        },
                    );
                });
            });

        let chat_frame = egui::Frame::none()
            .fill(eff_bg)
            .stroke(Stroke::new(
                if self.is_connected { 1.5_f32 } else { 2.5_f32 },
                if self.is_connected { GOLD_DIM } else { Color32::from_rgb(225, 55, 65) },
            ))
            .inner_margin(8.0);

        if self.limit_view_range {
            let map_h = (352.0 * self.zoom)
                .clamp(240.0, (ctx.screen_rect().height() * 0.58).max(260.0));
            egui::TopBottomPanel::top("map_viewport_panel")
                .exact_height(map_h)
                .frame(
                    egui::Frame::none()
                        .fill(Color32::BLACK)
                        .stroke(Stroke::new(
                            if self.is_connected { 1.5_f32 } else { 2.5_f32 },
                            if self.is_connected { accent } else { Color32::from_rgb(225, 55, 65) },
                        )),
                )
                .show(ctx, |ui| {
                    self.render_isometric_world(ui);
                });
            egui::CentralPanel::default()
                .frame(chat_frame)
                .show(ctx, |ui| {
                    self.render_chat_panel_contents(ui);
                });
        } else {
            let chat_min_h = 180.0_f32;
            let chat_max_h = (ctx.screen_rect().height() * 0.65).max(240.0);
            let chat_h = self.chat_panel_height.clamp(chat_min_h, chat_max_h);
            egui::TopBottomPanel::bottom("chat_panel")
                .exact_height(chat_h)
                .resizable(false)
                .frame(chat_frame)
                .show(ctx, |ui| {
                    self.render_chat_panel_contents(ui);
                });
            egui::TopBottomPanel::bottom("chat_resize_handle")
                .exact_height(5.0)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    let resp = ui.allocate_response(ui.available_size(), Sense::drag());
                    let y = resp.rect.center().y;
                    let stroke_color = if resp.hovered() || resp.dragged() {
                        ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
                        accent
                    } else {
                        GOLD_DIM
                    };
                    ui.painter()
                        .hline(resp.rect.x_range(), y, Stroke::new(1.0_f32, stroke_color));
                    if resp.dragged() && ui.input(|i| i.pointer.primary_down()) {
                        self.chat_panel_height =
                            (chat_h - resp.drag_delta().y).clamp(chat_min_h, chat_max_h);
                    }
                });
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::none()
                        .fill(self.ui_bg())
                        .stroke(Stroke::new(
                            if self.is_connected { 2.0_f32 } else { 2.5_f32 },
                            if self.is_connected { accent } else { Color32::from_rgb(225, 55, 65) },
                        )),
                )
                .show(ctx, |ui| {
                    self.render_isometric_world(ui);
                });
        }
    }

    fn render_whisper_pounce_toolbar(&mut self, ui: &mut egui::Ui, peer: &str) {
        let wire = wire_player_name(peer);
        let watched = self
            .pounce
            .entries()
            .any(|e| e.name.eq_ignore_ascii_case(peer) || wire_player_name(&e.name).eq_ignore_ascii_case(&wire));
        ui.horizontal_wrapped(|ui| {
            if ui.small_button("Look").on_hover_text(format!("Look at {peer}")).clicked() {
                self.request_player_look(&wire);
                self.focus_chat_input = true;
            }
            if ui.small_button("Join").on_hover_text(format!("Join {peer}")).clicked() {
                self.send_raw_cmd(&format!("join {wire}"));
                self.focus_chat_input = true;
            }
            if ui.small_button("Summon").on_hover_text(format!("Summon {peer}")).clicked() {
                self.send_raw_cmd(&format!("summon {wire}"));
                self.focus_chat_input = true;
            }
            if ui.small_button("Follow").on_hover_text(format!("Follow {peer}")).clicked() {
                self.send_raw_cmd(&format!("follow {wire}"));
                self.focus_chat_input = true;
            }
            if watched {
                if ui.small_button("-Pounce").on_hover_text(format!("Remove {peer} from Pounce")).clicked() {
                    self.pounce.remove_watch(peer);
                    self.save_config();
                    if self.is_connected {
                        self.send_raw_cmd(&format!("pounce deluser {wire}"));
                    }
                    self.focus_chat_input = true;
                }
            } else if ui.small_button("+Pounce").on_hover_text(format!("Add {peer} to Pounce")).clicked() {
                self.pounce.add_watch(peer.to_owned());
                self.save_config();
                if self.is_connected {
                    self.send_raw_cmd(&format!("pounce adduser {wire}"));
                }
                self.focus_chat_input = true;
            }
            if ui.small_button("Ignore").on_hover_text(format!("Ignore / Unignore {peer}")).clicked() {
                self.send_raw_cmd(&format!("ignore {wire}"));
                self.focus_chat_input = true;
            }
            if ui.small_button("History").on_hover_text(format!("View whisper logs with {peer}")).clicked() {
                if self.shared_logs_mode {
                    self.log_viewer_char = SHARED_LOG_GROUP.to_owned();
                }
                self.open_log_viewer();
                let target_key = peer_shortname_key(peer);
                if let Some(p) = self
                    .log_viewer_files
                    .iter()
                    .find(|p| {
                        p.file_stem()
                            .and_then(|s| s.to_str())
                            .is_some_and(|s| s.eq_ignore_ascii_case(&wire) || peer_shortname_key(s) == target_key)
                    })
                    .cloned()
                {
                    self.load_log_file_into_viewer(p);
                }
            }
            if ui
                .small_button("↗ Pop Out")
                .on_hover_text(format!("Detach {peer} whisper into its own window"))
                .clicked()
            {
                if let Some(w) = self
                    .whispers
                    .iter_mut()
                    .find(|w| w.peer.eq_ignore_ascii_case(peer))
                {
                    w.detached = true;
                    w.unread = 0;
                }
                if self
                    .active_chat_tab
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(peer))
                {
                    self.main_chat_unread = 0;
                    self.switch_chat_tab(None);
                    self.focus_chat_input = true;
                }
            }
        });
    }

    fn render_chat_panel_contents(&mut self, ui: &mut egui::Ui) {
        let accent = self.ui_accent();
        let mut chat_style = (**ui.style()).clone();
        let body_family = chat_style.text_styles[&egui::TextStyle::Body].family.clone();
        chat_style.text_styles.insert(
            egui::TextStyle::Body,
            FontId::new((self.chat_font_size as f32).round(), body_family),
        );
        chat_style.visuals.override_text_color = Some(self.colors.chat_text);
        ui.set_style(chat_style);
        if !self.is_connected {
            egui::Frame::none()
                .fill(Color32::from_rgb(74, 24, 32))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(225, 70, 80)))
                .rounding(4.0)
                .inner_margin(egui::Margin::symmetric(8.0, 5.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let title = if self.reconnecting {
                            if self.connecting_stage.is_empty() {
                                "Reconnecting to Furcadia…".to_owned()
                            } else {
                                format!("Reconnecting to Furcadia… ({})", self.connecting_stage)
                            }
                        } else {
                            "Disconnected from Furcadia".to_owned()
                        };
                        ui.label(RichText::new(title).strong().color(Color32::WHITE));
                        if !self.reconnecting && ui.button("Reconnect").clicked() {
                            self.reconnect_active_character();
                        }
                    });
                });
        }
                // Chat + Whisper Tabs bar (horizontally scrollable when many whispers are open)
                let mut close_peer: Option<String> = None;
                let tabs_max_w = ui.available_width().max(0.0);
                let tab_font_sz = (self.font_size as f32).clamp(11.0, 24.0).round();
                let tab_regular_font = FontId::new(tab_font_sz, self.ui_font_family(false));
                let tab_bold_font = FontId::new(tab_font_sz, self.ui_font_family(true));
                ui.scope(|ui| {
                    // Use a solid scrollbar so it gets its own strip below the tabs.
                    ui.spacing_mut().scroll.floating = false;
                    ui.spacing_mut().button_padding = Vec2::new(8.0, 4.0);
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let tab_text_h = ui.fonts(|f| {
                        f.row_height(&tab_regular_font)
                            .max(f.row_height(&tab_bold_font))
                    });
                    let tab_btn_h = (tab_text_h + 10.0).max(24.0).ceil();
                    ui.spacing_mut().interact_size.y = tab_btn_h;
                    let scroll_bar_h = ui.spacing().scroll.bar_width
                        + ui.spacing().scroll.bar_inner_margin
                        + ui.spacing().scroll.bar_outer_margin;
                    let strip_h = (tab_btn_h + scroll_bar_h + 6.0).max(36.0);
                    ui.allocate_ui(Vec2::new(tabs_max_w, strip_h), |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("whisper_tabs_scroll")
                            .auto_shrink([false, false])
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                            )
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let main_sel = self.active_chat_tab.is_none();
                                    let main_label = if self.main_chat_unread > 0 && !main_sel {
                                        format!("Main Chat ({})", self.main_chat_unread)
                                    } else {
                                        "Main Chat".to_owned()
                                    };
                                    let main_clicked = if !main_sel && self.main_chat_unread > 0 {
                                        ui.add(
                                            egui::Button::new(
                                                RichText::new(main_label)
                                                    .font(tab_bold_font.clone())
                                                    .strong()
                                                    .color(self.colors.tab_unread_text),
                                            )
                                            .fill(self.colors.tab_unread_bg)
                                            .stroke(Stroke::new(1.5_f32, self.colors.tab_unread_bg))
                                            .rounding(4.0)
                                            .min_size(Vec2::new(0.0, tab_btn_h)),
                                        )
                                        .clicked()
                                    } else {
                                        ui.add(
                                            egui::Button::new(if main_sel {
                                                RichText::new(main_label)
                                                    .font(tab_bold_font.clone())
                                                    .strong()
                                                    .color(self.colors.tab_active_text)
                                            } else {
                                                RichText::new(main_label)
                                                    .font(tab_regular_font.clone())
                                                    .color(self.colors.tab_inactive_text)
                                            })
                                            .fill(if main_sel {
                                                self.colors.tab_active_bg
                                            } else {
                                                self.colors.tab_inactive_bg
                                            })
                                            .rounding(4.0)
                                            .min_size(Vec2::new(0.0, tab_btn_h)),
                                        )
                                        .clicked()
                                    };
                                    if main_clicked {
                                        self.main_chat_unread = 0;
                                        self.switch_chat_tab(None);
                                        self.focus_chat_input = true;
                                    }

                                    let mut whisper_tab_rects: Vec<(usize, Rect)> = Vec::new();
                                    let mut activate_peer: Option<String> = None;
                                    for (idx, w) in self.whispers.iter_mut().enumerate() {
                                        if w.detached {
                                            continue;
                                        }
                                        let sel = self
                                            .active_chat_tab
                                            .as_deref()
                                            .is_some_and(|a| a.eq_ignore_ascii_case(&w.peer));
                                        let label = if w.unread > 0 && !sel {
                                            format!("{} ({})", name_display_clean(&w.peer), w.unread)
                                        } else {
                                            name_display_clean(&w.peer)
                                        };
                                        ui.spacing_mut().item_spacing.x = 2.0;
                                        let resp = if !sel && w.unread > 0 {
                                            ui.add(
                                                egui::Button::new(
                                                    RichText::new(label)
                                                        .font(tab_bold_font.clone())
                                                        .strong()
                                                        .color(self.colors.tab_unread_text),
                                                )
                                                .fill(self.colors.tab_unread_bg)
                                                .stroke(Stroke::new(
                                                    1.5_f32,
                                                    self.colors.tab_unread_bg,
                                                ))
                                                .rounding(4.0)
                                                .min_size(Vec2::new(0.0, tab_btn_h))
                                                .sense(Sense::click_and_drag()),
                                            )
                                        } else if sel {
                                            ui.add(
                                                egui::Button::new(
                                                    RichText::new(label)
                                                        .font(tab_bold_font.clone())
                                                        .strong()
                                                        .color(self.colors.tab_active_text),
                                                )
                                                .fill(self.colors.tab_active_bg)
                                                .rounding(4.0)
                                                .min_size(Vec2::new(0.0, tab_btn_h))
                                                .sense(Sense::click_and_drag()),
                                            )
                                        } else {
                                            ui.add(
                                                egui::Button::new(
                                                    RichText::new(label)
                                                        .font(tab_regular_font.clone())
                                                        .color(self.colors.tab_inactive_text),
                                                )
                                                .fill(self.colors.tab_inactive_bg)
                                                .rounding(4.0)
                                                .min_size(Vec2::new(0.0, tab_btn_h))
                                                .sense(Sense::click_and_drag()),
                                            )
                                        };
                                        if resp.drag_started() {
                                            self.dragging_whisper_tab = Some(idx);
                                        } else if resp.clicked() {
                                            w.unread = 0;
                                            activate_peer = Some(w.peer.clone());
                                            self.focus_chat_input = true;
                                        }
                                        whisper_tab_rects.push((idx, resp.rect));
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new("×")
                                                        .font(tab_regular_font.clone()),
                                                )
                                                .rounding(4.0)
                                                .min_size(Vec2::new(22.0, tab_btn_h)),
                                            )
                                            .on_hover_text("Close tab")
                                            .clicked()
                                        {
                                            close_peer = Some(w.peer.clone());
                                        }
                                    }
                                    if let Some(peer) = activate_peer {
                                        self.switch_chat_tab(Some(peer));
                                    }
                                    if !ui.input(|i| i.pointer.any_down()) {
                                        self.dragging_whisper_tab = None;
                                    } else if let (Some(from_idx), Some(pos)) = (
                                        self.dragging_whisper_tab,
                                        ui.input(|i| i.pointer.interact_pos()),
                                    ) {
                                        if let Some(&(to_idx, _)) = whisper_tab_rects
                                            .iter()
                                            .find(|(i, r)| *i != from_idx && r.contains(pos))
                                        {
                                            if from_idx < self.whispers.len()
                                                && to_idx < self.whispers.len()
                                            {
                                                self.whispers.swap(from_idx, to_idx);
                                                self.dragging_whisper_tab = Some(to_idx);
                                            }
                                        }
                                    }
                                });
                            });
                    });
                });

                if let Some(peer) = close_peer {
                    self.close_whisper_tab(&peer);
                }

                ui.separator();
                // Chat Formatting Toolbar (AFK, B, I, U, Smileys, Font)
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(self.afk_active, "AFK")
                        .on_hover_text("Toggle AFK")
                        .clicked()
                    {
                        self.afk_active = !self.afk_active;
                        self.send_raw_cmd("afk");
                    }
                    ui.separator();
                    if ui.small_button(RichText::new("B").strong()).on_hover_text("Bold <b>...</b>").clicked() {
                        self.chat_input.push_str("<b></b>");
                    }
                    if ui.small_button(RichText::new("I").italics()).on_hover_text("Italic <i>...</i>").clicked() {
                        self.chat_input.push_str("<i></i>");
                    }
                    if ui.small_button(RichText::new("U").underline()).on_hover_text("Underline <u>...</u>").clicked() {
                        self.chat_input.push_str("<u></u>");
                    }
                    if ui
                        .selectable_label(self.smiley_picker_open, ":)")
                        .on_hover_text("Insert Smiley")
                        .clicked()
                    {
                        self.smiley_picker_open = !self.smiley_picker_open;
                    }
                    if ui
                        .selectable_label(self.link_builder_open, "Link")
                        .on_hover_text("Create a clickable link for chat")
                        .clicked()
                    {
                        self.link_builder_open = !self.link_builder_open;
                    }
                    let dice = if self.dice_roll.trim().is_empty() {
                        "1d6"
                    } else {
                        self.dice_roll.trim()
                    };
                    let dice_ready = self
                        .last_dice_roll_at
                        .is_none_or(|at| at.elapsed() >= Duration::from_secs(3));
                    let dice_button = ui
                        .add_enabled(
                            self.is_connected && dice_ready,
                            egui::Button::new(format!("Roll {dice}")),
                        )
                        .on_hover_text(if dice_ready {
                            format!("Roll {dice} (configurable in Settings → Whispers & Chat)")
                        } else {
                            "Wait a few seconds between dice rolls".to_owned()
                        });
                    if dice_button.clicked() {
                        self.last_dice_roll_at = Some(Instant::now());
                        let _ = self.to_net.send(UiToNet::Send(ClientCommand::Say(format!(
                            "roll {dice}"
                        ))));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .selectable_label(self.font_popup_open, "Font")
                            .on_hover_text("Select chat font & size")
                            .clicked()
                        {
                            self.font_popup_open = !self.font_popup_open;
                        }
                        if ui.small_button("Logs").on_hover_text("View saved logs").clicked() {
                            self.open_log_viewer();
                        }
                    });
                });
                if self.link_builder_open {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("URL:").size(12.0));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.link_url_input)
                                .hint_text("https://... or furc://...")
                                .desired_width(180.0),
                        );
                        ui.label(RichText::new("Text:").size(12.0));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.link_label_input)
                                .hint_text("optional label")
                                .desired_width(120.0),
                        );
                        if ui.small_button("Insert").clicked() {
                            let url = self.link_url_input.trim();
                            if !url.is_empty() {
                                let label = self.link_label_input.trim();
                                let formatted = if label.is_empty() {
                                    url.to_owned()
                                } else {
                                    format!("<a href='{url}'>{label}</a>")
                                };
                                if !self.chat_input.is_empty() && !self.chat_input.ends_with(' ') {
                                    self.chat_input.push(' ');
                                }
                                self.chat_input.push_str(&formatted);
                                self.link_url_input.clear();
                                self.link_label_input.clear();
                                self.link_builder_open = false;
                            }
                        }
                    });
                }
                if self.smiley_picker_open {
                    ui.horizontal_wrapped(|ui| {
                        let fallback_labels = [
                            ('A', ":)"), ('B', ":D"), ('C', ";)"), ('D', ":("),
                            ('E', ":P"), ('F', ":O"), ('G', ">:("), ('H', "B)"),
                            ('I', ":$"), ('J', "O:)"), ('K', ">:)"), ('L', "<3"),
                            ('M', "</3"), ('N', "Rose"), ('O', "<3"), ('P', "#SP"),
                            ('Q', "#SQ"), ('R', "Zzz"), ('S', ":/"), ('T', "XD"),
                            ('U', ":*"), ('V', "8|"), ('W', ":S"), ('X', "-_-"),
                            ('Y', "+1"), ('Z', "-1"),
                        ];
                        for (ch, fallback_label) in fallback_labels {
                            let code = format!("#S{ch}");
                            let tex_opt = smiley_index_for_code(ch)
                                .and_then(|idx| self.smiley_textures.get(idx))
                                .and_then(|t| t.as_ref());
                            let resp = if let Some(tex) = tex_opt {
                                let (rect, resp) = ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
                                let bg = if resp.hovered() {
                                    Color32::from_rgb(245, 240, 255)
                                } else {
                                    Color32::from_rgb(218, 212, 232)
                                };
                                let border = if resp.hovered() { accent } else { GOLD_DIM };
                                let p = ui.painter();
                                p.rect_filled(rect, 4.0, bg);
                                p.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, border));
                                let img_rect = rect.shrink(3.0);
                                p.image(
                                    tex.id(),
                                    img_rect,
                                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                                resp
                            } else {
                                ui.add(
                                    egui::Button::new(fallback_label)
                                        .fill(Color32::from_rgb(218, 212, 232))
                                        .min_size(Vec2::new(26.0, 26.0)),
                                )
                            };
                            if resp.on_hover_text(&code).clicked() {
                                self.chat_input.push_str(&code);
                                self.smiley_picker_open = false;
                            }
                        }
                    });
                }
                if self.font_popup_open {
                    ui.horizontal(|ui| {
                        for fam in ["Proportional", "Monospace", "Fixedsys"] {
                            if ui.radio(self.font_family == fam, fam).clicked() {
                                self.font_family = fam.to_owned();
                                self.save_config();
                            }
                        }
                        if ui.small_button("-").clicked() && self.font_size > 9 {
                            self.font_size -= 1;
                            self.save_config();
                        }
                        ui.label(format!("{}px", self.font_size));
                        if ui.small_button("+").clicked() && self.font_size < 24 {
                            self.font_size += 1;
                            self.save_config();
                        }
                    });
                }

                if let Some(peer) = self.active_chat_tab.clone() {
                    ui.separator();
                    self.render_whisper_pounce_toolbar(ui, &peer);
                }
                ui.separator();
                let total_avail_h = ui.available_height();
                let line_h = ui.text_style_height(&egui::TextStyle::Body);
                let input_w = (ui.available_width() - 72.0).max(40.0);
                let spelling_h = if self.spell_misspelled.is_empty() { 0.0 } else { 24.0 };
                let bottom_chrome = ui.spacing().item_spacing.y * 4.0 + 24.0;
                let min_input_h = (line_h + 12.0).max(ui.spacing().interact_size.y + 4.0);
                let max_input_h = (total_avail_h - 64.0 - spelling_h - bottom_chrome).max(min_input_h);
                let typed_h = if self.chat_input.is_empty() {
                    line_h
                } else {
                    let job = build_spellcheck_layout_job(
                        ui,
                        &self.chat_input,
                        (input_w - 12.0).max(40.0),
                        &self.spell_misspelled,
                        self.colors.input_text,
                    );
                    ui.fonts(|f| f.layout_job(job)).size().y
                };
                let input_h = (typed_h + 12.0).clamp(min_input_h, max_input_h);
                let remaining_for_chat = (total_avail_h - input_h - spelling_h - bottom_chrome).max(0.0);
                let available_h = remaining_for_chat;
                if (self.last_chat_avail_h - available_h).abs() > 0.5 && self.chat_pinned_to_bottom {
                    self.chat_scroll_to_bottom = true;
                }
                self.last_chat_avail_h = available_h;
                let active_peer = self.active_chat_tab.clone();
                let self_name = self.active_character_name();
                let self_name_key = shortname_key(&self_name);
                let mut chat_action: Option<ChatClickAction> = None;
                let player_badges = self.current_player_badge_textures(ui.ctx());
                let scroll_d = std::mem::take(&mut self.chat_scroll_delta);
                if scroll_d > 0.5 {
                    self.chat_pinned_to_bottom = false;
                    self.chat_scroll_to_bottom = false;
                }
                let scroll_to_bottom = std::mem::take(&mut self.chat_scroll_to_bottom);
                let scroll_out = egui::ScrollArea::vertical()
                    .id_salt(("chat_scroll", active_peer.as_deref().unwrap_or("main")))
                    .auto_shrink([false, false])
                    .min_scrolled_height(remaining_for_chat)
                    .max_height(available_h)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.set_min_height(remaining_for_chat);
                        if scroll_d.abs() > 0.5 {
                            ui.scroll_with_delta(Vec2::new(0.0, scroll_d));
                        }
                        if let Some(peer) = &active_peer {
                            if let Some(w) = self
                                .whispers
                                .iter()
                                .find(|w| w.peer.eq_ignore_ascii_case(peer))
                            {
                                if w.history_lines.is_empty() && w.lines.is_empty() {
                                    ui.label(
                                        RichText::new(format!("Whispering with {}...", w.peer))
                                            .italics()
                                            .color(Color32::GRAY),
                                    );
                                } else {
                                    render_whisper_history_lines(
                                        ui,
                                        &w.history_lines,
                                        !w.lines.is_empty(),
                                        &self_name,
                                        &w.peer,
                                        &mut chat_action,
                                        &self.smiley_textures,
                                        &self.desctag_textures,
                                        &self.chat_img_textures,
                                        &self.player_badge_colors,
                                        &player_badges,
                                        &self_name_key,
                                        &self.colors,
                                    );
                                    for line in &w.lines {
                                        render_chat_line(
                                            ui,
                                            line,
                                            &mut chat_action,
                                            &self.smiley_textures,
                                            &self.desctag_textures,
                                            &self.chat_img_textures,
                                            &self.player_badge_colors,
                                            &player_badges,
                                            &self_name_key,
                                            &self.colors,
                                        );
                                    }
                                }
                            }
                        } else {
                            for line in &self.chat_log {
                                render_chat_line(
                                    ui,
                                    line,
                                    &mut chat_action,
                                    &self.smiley_textures,
                                    &self.desctag_textures,
                                    &self.chat_img_textures,
                                    &self.player_badge_colors,
                                    &player_badges,
                                    &self_name_key,
                                    &self.colors,
                                );
                            }
                        }
                        if scroll_to_bottom {
                            ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                        }
                    });
                let at_bottom = scroll_out.state.offset.y + scroll_out.inner_rect.height() + 24.0
                    >= scroll_out.content_size.y;
                if at_bottom || scroll_to_bottom {
                    self.chat_pinned_to_bottom = true;
                } else if ui.input(|i| i.raw_scroll_delta.y > 0.0 || i.pointer.any_down()) {
                    self.chat_pinned_to_bottom = false;
                }
                if let Some(act) = chat_action {
                    self.handle_chat_click_action(act);
                }

                if let Some(dirty_at) = self.spell_dirty_at {
                    if dirty_at.elapsed() >= std::time::Duration::from_millis(200) {
                        self.spell_dirty_at = None;
                        self.spell_seq = self.spell_seq.wrapping_add(1);
                        let _ = self.to_net.send(UiToNet::SpellCheck {
                            seq: self.spell_seq,
                            text: self.chat_input.clone(),
                            custom_dict: self.spellcheck_dict_with_names(),
                        });
                    } else {
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
                    }
                }
                if !self.spell_misspelled.is_empty() {
                    let mut replace_action: Option<(usize, usize, String)> = None;
                    let mut add_dict_word: Option<String> = None;
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Spelling:")
                                .size(11.0)
                                .color(Color32::from_rgb(245, 130, 130)),
                        );
                        for m in self.spell_misspelled.iter().take(2) {
                            ui.label(
                                RichText::new(format!("'{}'", m.word))
                                    .size(11.0)
                                    .color(Color32::from_rgb(245, 190, 190)),
                            );
                            for sug in m.suggestions.iter().take(3) {
                                if ui.small_button(sug).clicked() {
                                    replace_action = Some((m.start, m.end, sug.clone()));
                                }
                            }
                            if ui
                                .small_button("+Dict")
                                .on_hover_text(format!("Add '{}' to dictionary", m.word))
                                .clicked()
                            {
                                add_dict_word = Some(m.word.clone());
                            }
                        }
                    });
                    if let Some((start, end, rep)) = replace_action {
                        if start <= end && end <= self.chat_input.len()
                            && self.chat_input.is_char_boundary(start)
                            && self.chat_input.is_char_boundary(end)
                        {
                            self.chat_input.replace_range(start..end, &rep);
                            self.spell_dirty_at = Some(Instant::now());
                        }
                    }
                    if let Some(w) = add_dict_word {
                        self.add_word_to_dictionary(&w);
                    }
                }
                ui.separator();
                let row_w = ui.available_width();
                ui.allocate_ui_with_layout(
                    Vec2::new(row_w, input_h),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let send_btn = egui::Button::new(RichText::new("Send").color(Color32::WHITE))
                            .fill(self.ui_panel())
                            .stroke(Stroke::new(1.0_f32, accent));
                        let send_clicked = ui.add(send_btn).clicked();
                    let chat_input_id = ui.make_persistent_id((
                        "main_chat_input",
                        active_peer.as_deref().unwrap_or("main"),
                    ));
                    if std::mem::take(&mut self.focus_chat_input) {
                        ui.memory_mut(|m| m.request_focus(chat_input_id));
                    }
                    let enter = ui.memory(|m| m.has_focus(chat_input_id))
                        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                    let misspelled_snapshot = self.spell_misspelled.clone();
                    let input_text_color = self.colors.input_text;
                    let mut layouter = move |ui: &egui::Ui, text: &str, wrap_width: f32| {
                        let job = build_spellcheck_layout_job(
                            ui,
                            text,
                            wrap_width,
                            &misspelled_snapshot,
                            input_text_color,
                        );
                        ui.fonts(|f| f.layout_job(job))
                    };
                    let row_input_w = (ui.available_width() - 8.0).max(40.0);
                    let resp = egui::ScrollArea::vertical()
                        .id_salt((
                            "main_chat_input_scroll",
                            active_peer.as_deref().unwrap_or("main"),
                        ))
                        .max_height(input_h)
                        .min_scrolled_height(input_h)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut self.chat_input)
                                    .id(chat_input_id)
                                    .desired_rows(1)
                                    .hint_text("")
                                    .desired_width(row_input_w)
                                    .layouter(&mut layouter),
                            )
                        })
                        .inner;
                    if resp.changed() {
                        if self.chat_input.trim().is_empty() {
                            self.spell_misspelled.clear();
                            self.spell_dirty_at = None;
                        } else {
                            self.spell_dirty_at = Some(Instant::now());
                        }
                    }
                    let mut ctx_replace: Option<(usize, usize, String)> = None;
                    let mut ctx_add_dict: Option<String> = None;
                    let ctx_misspelled = self.spell_misspelled.clone();
                    resp.context_menu(|ui| {
                        let state = egui::TextEdit::load_state(ui.ctx(), chat_input_id);
                        let sel_range = state.as_ref().and_then(|s| s.cursor.char_range());
                        let selected_text = sel_range.and_then(|r| {
                            let (a, b) = (r.primary.index.min(r.secondary.index), r.primary.index.max(r.secondary.index));
                            if a < b {
                                Some(self.chat_input.chars().skip(a).take(b - a).collect::<String>())
                            } else {
                                None
                            }
                        });
                        if ui
                            .add_enabled(selected_text.is_some(), egui::Button::new("Cut"))
                            .clicked()
                        {
                            if let (Some(txt), Some(r)) = (&selected_text, sel_range) {
                                ui.ctx().copy_text(txt.clone());
                                let a = r.primary.index.min(r.secondary.index);
                                let b = r.primary.index.max(r.secondary.index);
                                let byte_a = self.chat_input.char_indices().nth(a).map(|(i, _)| i).unwrap_or(self.chat_input.len());
                                let byte_b = self.chat_input.char_indices().nth(b).map(|(i, _)| i).unwrap_or(self.chat_input.len());
                                self.chat_input.replace_range(byte_a..byte_b, "");
                                if let Some(mut st) = state.clone() {
                                    let c = egui::text::CCursor::new(a);
                                    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(c)));
                                    st.store(ui.ctx(), chat_input_id);
                                }
                                self.spell_dirty_at = Some(Instant::now());
                            }
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled(selected_text.is_some(), egui::Button::new("Copy"))
                            .clicked()
                        {
                            if let Some(txt) = &selected_text {
                                ui.ctx().copy_text(txt.clone());
                            }
                            ui.close_menu();
                        }
                        if ui.button("Paste").clicked() {
                            if let Some(clip) = read_system_clipboard() {
                                let cleaned = clip.replace(['\r', '\n'], " ");
                                if let Some(r) = sel_range {
                                    let a = r.primary.index.min(r.secondary.index);
                                    let b = r.primary.index.max(r.secondary.index);
                                    let byte_a = self.chat_input.char_indices().nth(a).map(|(i, _)| i).unwrap_or(self.chat_input.len());
                                    let byte_b = self.chat_input.char_indices().nth(b).map(|(i, _)| i).unwrap_or(self.chat_input.len());
                                    self.chat_input.replace_range(byte_a..byte_b, &cleaned);
                                    if let Some(mut st) = state.clone() {
                                        let new_pos = a + cleaned.chars().count();
                                        let c = egui::text::CCursor::new(new_pos);
                                        st.cursor.set_char_range(Some(egui::text::CCursorRange::one(c)));
                                        st.store(ui.ctx(), chat_input_id);
                                    }
                                } else {
                                    self.chat_input.push_str(&cleaned);
                                }
                                self.spell_dirty_at = Some(Instant::now());
                            }
                            ui.close_menu();
                        }
                        if ui.button("Select All").clicked() {
                            let len_chars = self.chat_input.chars().count();
                            let mut st = state.unwrap_or_default();
                            st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(len_chars),
                            )));
                            st.store(ui.ctx(), chat_input_id);
                            ui.close_menu();
                        }
                        if !ctx_misspelled.is_empty() {
                            ui.separator();
                            for m in ctx_misspelled.iter().take(3) {
                                ui.label(
                                    RichText::new(format!("Spelling: {}", m.word))
                                        .small()
                                        .color(Color32::from_rgb(245, 130, 130)),
                                );
                                for sug in m.suggestions.iter().take(4) {
                                    if ui.button(format!("  -> {sug}")).clicked() {
                                        ctx_replace = Some((m.start, m.end, sug.clone()));
                                        ui.close_menu();
                                    }
                                }
                                if ui.button(format!("  + Add \"{}\" to dictionary", m.word)).clicked() {
                                    ctx_add_dict = Some(m.word.clone());
                                    ui.close_menu();
                                }
                            }
                        }
                    });
                    if let Some((start, end, rep)) = ctx_replace {
                        if start <= end && end <= self.chat_input.len()
                            && self.chat_input.is_char_boundary(start)
                            && self.chat_input.is_char_boundary(end)
                        {
                            self.chat_input.replace_range(start..end, &rep);
                            self.spell_dirty_at = Some(Instant::now());
                        }
                    }
                    if let Some(w) = ctx_add_dict {
                        self.add_word_to_dictionary(&w);
                    }
                    if send_clicked || enter {
                        self.send_chat_input();
                        resp.request_focus();
                    }
                });
    }

    fn render_isometric_world(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if !ctx.wants_keyboard_input() && self.is_connected {
            let mut move_dir = None;
            ctx.input(|i| {
                if i.key_pressed(egui::Key::Num7) {
                    move_dir = Some(7);
                } else if i.key_pressed(egui::Key::Num9) {
                    move_dir = Some(9);
                } else if i.key_pressed(egui::Key::Num1) {
                    move_dir = Some(1);
                } else if i.key_pressed(egui::Key::Num3) {
                    move_dir = Some(3);
                }
            });
            if let Some(dir) = move_dir {
                let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(dir)));
            }
        }
        let rect = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(rect, Sense::click_and_drag());

        if response.dragged() {
            self.cam_offset += response.drag_delta();
        }

        if self.limit_view_range && self.zoom < 0.95 {
            self.zoom = 0.95;
        }
        let bg_painter = ui.painter_at(rect);
        let bg_fill = if self.limit_view_range {
            Color32::BLACK
        } else {
            Color32::from_rgb(18, 22, 28)
        };
        bg_painter.rect_filled(rect, 0.0, bg_fill);

        let tile_w = 64.0 * self.zoom;
        let tile_h = 32.0 * self.zoom;
        let scale = self.zoom;
        let raw_anim_ms = ctx.input(|i| (i.time * 1000.0) as u64);
        let anim_ms = match self.anim_mode.as_str() {
            "Off" => 0,
            "Half" => raw_anim_ms / 2,
            _ => raw_anim_ms,
        };
        // Generous overdraw margin so tall trees, walls, roofs, and dream effects
        // anchored outside the viewport still render when they poke into view.
        let cull_rect = egui::Rect::from_min_max(
            Pos2::new(rect.min.x - tile_w * 8.0, rect.min.y - tile_h * 8.0),
            Pos2::new(rect.max.x + tile_w * 8.0, rect.max.y + tile_h * 16.0),
        );

        let cur_region = if let (Some(map), Some((sx, sy))) = (&self.world.map, self.world.self_pos)
        {
            map.region_at(sx as usize, sy as usize)
        } else {
            0
        };
        let self_elev = if let Some((sx, sy)) = self.world.self_pos {
            self.resolve_map_tile_elev(&ctx, cur_region, sx as usize, sy as usize, anim_ms)
        } else {
            (0, 0)
        };

        let origin = if let Some((sx, sy)) = self.world.self_pos {
            let iso = tile_to_screen(sx as f32, sy as f32, tile_w, tile_h);
            rect.center()
                - iso
                - Vec2::new(self_elev.0 as f32, self_elev.1 as f32) * scale
                + self.cam_offset
        } else if let Some(map) = &self.world.map {
            let iso =
                tile_to_screen(map.width as f32 * 0.5, map.height as f32 * 0.5, tile_w, tile_h);
            rect.center() - iso + self.cam_offset
        } else {
            rect.center() + self.cam_offset
        };

        let chat_clip_rect = if self.limit_view_range {
            self.world.self_pos.map(|(sx, sy)| {
                let self_center = origin
                    + tile_to_screen(sx as f32, sy as f32, tile_w, tile_h)
                    + Vec2::new(self_elev.0 as f32, self_elev.1 as f32) * scale;
                egui::Rect::from_center_size(
                    self_center,
                    Vec2::new(624.0 * scale, 352.0 * scale),
                )
            })
        } else {
            None
        };
        let painter = if let Some(clip) = chat_clip_rect {
            bg_painter.rect_stroke(
                clip.intersect(rect),
                0.0,
                Stroke::new(1.0_f32, Color32::from_rgb(48, 42, 70)),
            );
            ui.painter_at(rect.intersect(clip))
        } else {
            bg_painter
        };

        struct TileDrawEntry {
            x: usize,
            y: usize,
            center: Pos2,
            tile_top_left: Pos2,
            floor_id: u16,
            item_id: u16,
            wall_nw: u16,
            wall_ne: u16,
            upper_nw: u16,
            upper_ne: u16,
            effect_id: u16,
            floor_elev: (i16, i16),
        }

        let mut tiles_to_draw: Vec<TileDrawEntry> = Vec::new();
        if let Some(map) = &self.world.map {
            let resolve_wall_byte = |mx: isize, my: isize, is_ne: bool| -> u16 {
                if mx < 0 || my < 0 {
                    return 0;
                }
                let (ux, uy) = (mx as usize, my as usize);
                if ux >= map.width || uy >= map.height {
                    return 0;
                }
                let raw = map.wall_at(ux, uy);
                let byte = if is_ne { (raw >> 8) & 0xff } else { raw & 0xff };
                if byte == 0 {
                    0
                } else {
                    self.world.resolve_tile_layer(
                        cur_region,
                        map.region_at(ux, uy),
                        RegionLayer::Wall,
                        byte,
                    )
                }
            };

            for y in 0..map.height {
                let odd = y & 1;
                for x in 0..map.width {
                    let center = origin + tile_to_screen(x as f32, y as f32, tile_w, tile_h);
                    if let Some(clip) = chat_clip_rect {
                        let chat_cull = egui::Rect::from_min_max(
                            Pos2::new(clip.min.x - tile_w * 8.0, clip.min.y - tile_h * 8.0),
                            Pos2::new(clip.max.x + tile_w * 8.0, clip.max.y + tile_h * 16.0),
                        );
                        if !chat_cull.contains(center) {
                            continue;
                        }
                    }
                    if !cull_rect.contains(center) {
                        continue;
                    }
                    let tile_region = map.region_at(x, y);
                    let raw_wall = map.wall_at(x, y);
                    let raw_nw = raw_wall & 0xff;
                    let raw_ne = (raw_wall >> 8) & 0xff;
                    let wall_nw = if raw_nw > 0 {
                        self.world.resolve_tile_layer(
                            cur_region,
                            tile_region,
                            RegionLayer::Wall,
                            raw_nw,
                        )
                    } else {
                        0
                    };
                    let wall_ne = if raw_ne > 0 {
                        self.world.resolve_tile_layer(
                            cur_region,
                            tile_region,
                            RegionLayer::Wall,
                            raw_ne,
                        )
                    } else {
                        0
                    };
                    let (ul_x, ul_y, ur_x, ur_y) = if odd != 0 {
                        (x as isize - 1, y as isize - 1, x as isize, y as isize - 1)
                    } else {
                        (x as isize, y as isize - 1, x as isize + 1, y as isize - 1)
                    };
                    let upper_nw = resolve_wall_byte(ul_x, ul_y, true);
                    let upper_ne = resolve_wall_byte(ur_x, ur_y, false);
                    tiles_to_draw.push(TileDrawEntry {
                        x,
                        y,
                        center,
                        tile_top_left: center - Vec2::new(33.0, 79.0) * scale,
                        floor_id: self.world.resolve_tile_layer(
                            cur_region,
                            tile_region,
                            RegionLayer::Floor,
                            map.floor_at(x, y),
                        ),
                        item_id: self.world.resolve_tile_layer(
                            cur_region,
                            tile_region,
                            RegionLayer::Item,
                            map.item_at(x, y),
                        ),
                        wall_nw,
                        wall_ne,
                        upper_nw,
                        upper_ne,
                        effect_id: self.world.resolve_tile_layer(
                            cur_region,
                            tile_region,
                            RegionLayer::Effect,
                            map.effect_at(x, y),
                        ),
                        floor_elev: (0, 0),
                    });
                }
            }
        } else {
            let (cx, cy) = self.world.self_pos.unwrap_or((20, 20));
            let min_x = cx.saturating_sub(18) as usize;
            let max_x = (cx as usize) + 18;
            let min_y = cy.saturating_sub(24) as usize;
            let max_y = (cy as usize) + 24;
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    let center = origin + tile_to_screen(x as f32, y as f32, tile_w, tile_h);
                    if !cull_rect.contains(center) {
                        continue;
                    }
                    tiles_to_draw.push(TileDrawEntry {
                        x,
                        y,
                        center,
                        tile_top_left: center - Vec2::new(33.0, 79.0) * scale,
                        floor_id: 0,
                        item_id: 0,
                        wall_nw: 0,
                        wall_ne: 0,
                        upper_nw: 0,
                        upper_ne: 0,
                        effect_id: 0,
                        floor_elev: (0, 0),
                    });
                }
            }
        }

        // Pass 1: row-major floor rendering with AnchorX=33, AnchorY=79 cell_origin.
        let mut floor_elevs: HashMap<(usize, usize), (i16, i16)> = HashMap::new();
        let mut tile_elevs: HashMap<(usize, usize), (i16, i16)> = HashMap::new();
        let mut any_animated = false;
        for tile in &mut tiles_to_draw {
            // Floor 161 is the DreamEd "no floor" placeholder (a pit sprite in the default
            // `floors.fox`); Furcadia renders it as empty black unless the dream defines its own.
            if tile.floor_id == 161
                && !self
                    .floors_cache
                    .dream_archive
                    .as_ref()
                    .is_some_and(|f| f.resolve_shape(161, 0).is_some())
            {
                continue;
            }
            if let Some(shape) = self.resolve_floor_layers(
                &ctx,
                tile.floor_id,
                tile.x,
                tile.y,
                anim_ms,
            ) {
                any_animated |= shape.is_animated;
                tile.floor_elev = shape.elev;
                if shape.elev != (0, 0) {
                    floor_elevs.insert((tile.x, tile.y), shape.elev);
                    tile_elevs.insert((tile.x, tile.y), shape.elev);
                }
                let mut floor_shadows = Vec::new();
                let mut floor_sprites = Vec::new();
                collect_shape_slot_ops(
                    &shape.layers,
                    shape.alpha,
                    0..4,
                    tile.tile_top_left,
                    scale,
                    (0, 0),
                    &mut floor_shadows,
                    &mut floor_sprites,
                );
                emit_map_draw_ops(&painter, &floor_shadows);
                emit_map_draw_ops(&painter, &floor_sprites);
            } else {
                let diamond = [
                    Pos2::new(tile.center.x, tile.center.y - tile_h * 0.5),
                    Pos2::new(tile.center.x + tile_w * 0.5, tile.center.y),
                    Pos2::new(tile.center.x, tile.center.y + tile_h * 0.5),
                    Pos2::new(tile.center.x - tile_w * 0.5, tile.center.y),
                ];
                let floor_color = if tile.floor_id == 0 {
                    Color32::from_rgb(28, 34, 42)
                } else {
                    let [r, g, b] = self
                        .palette
                        .rgb(((tile.floor_id.wrapping_mul(17)) % 240 + 10) as u8);
                    Color32::from_rgb(r, g, b)
                };
                painter.add(egui::Shape::convex_polygon(
                    diamond.to_vec(),
                    floor_color,
                    Stroke::new(0.6_f32, Color32::from_rgba_unmultiplied(0, 0, 0, 70)),
                ));
            }
        }

        let portal_states: HashMap<(u16, u16), i32> = self
            .world
            .dream_labels
            .iter()
            .map(|(&pos, dl)| (pos, dl.state_flag))
            .collect();

        let mut sorted_avatars: Vec<_> = self.world.avatars.values().cloned().collect();
        sorted_avatars.sort_by_key(|a| (a.y, a.x, a.uid));

        let mut hitboxes: Vec<AvatarHitBox> = Vec::new();
        let mut name_badges: Vec<NameBadge> = Vec::new();
        let mut av_idx = 0usize;
        let mut shadow_ops: Vec<MapDrawOp> = Vec::new();
        let mut fg_ops: Vec<MapDrawOp> = Vec::new();
        let thresh = self.world.region_defaults.indoor_threshold;

        // Pass 2: depth-sorted junctions, walls, items (slots 0..2 & 2..4), effects, and avatars.
        for tile in &tiles_to_draw {
            while av_idx < sorted_avatars.len() {
                let av = &sorted_avatars[av_idx];
                let ay = av.y as usize;
                let ax = av.x as usize;
                if (ay, ax) >= (tile.y, tile.x) {
                    break;
                }
                let hidden = self.world.map.as_ref().is_some_and(|m| {
                    ax < m.width && ay < m.height && {
                        let tr = m.region_at(ax, ay);
                        tr >= thresh && cur_region >= thresh && tr != cur_region
                    }
                });
                if !hidden {
                    let av_elev = tile_elevs.get(&(ax, ay)).copied().unwrap_or((0, 0));
                    let pos = origin
                        + tile_to_screen(av.x as f32, av.y as f32, tile_w, tile_h)
                        + Vec2::new(av_elev.0 as f32, av_elev.1 as f32) * scale;
                    let in_chat_view = chat_clip_rect
                        .is_none_or(|clip| clip.expand2(Vec2::new(16.0 * scale, 16.0 * scale)).contains(pos));
                    if in_chat_view && cull_rect.contains(pos) {
                        self.collect_avatar_ops(
                            &ctx,
                            pos,
                            tile_h,
                            scale,
                            anim_ms,
                            av,
                            &mut hitboxes,
                            &mut name_badges,
                            &mut shadow_ops,
                            &mut fg_ops,
                            &mut any_animated,
                        );
                    }
                }
                av_idx += 1;
            }

            let (group_or, j_mask) = compute_junction_state(
                tile.upper_nw,
                tile.upper_ne,
                tile.wall_nw,
                tile.wall_ne,
            );
            if let Some(cs) = self.resolve_junction_sprite(&ctx, group_or, j_mask) {
                if !cs.is_one_by_one {
                    push_cached_sprite_op(&mut fg_ops, tile.tile_top_left, (0, -16), scale, &cs);
                }
            }

            if tile.wall_nw > 0 || tile.wall_ne > 0 {
                let diamond = [
                    Pos2::new(tile.center.x, tile.center.y - tile_h * 0.5),
                    Pos2::new(tile.center.x + tile_w * 0.5, tile.center.y),
                    Pos2::new(tile.center.x, tile.center.y + tile_h * 0.5),
                    Pos2::new(tile.center.x - tile_w * 0.5, tile.center.y),
                ];
                for (edge_byte, fox_dir, anchor_dx, _edge_line) in [
                    (tile.wall_nw, 9u8, -16i16, [diamond[3], diamond[0]]),
                    (tile.wall_ne, 7u8, 16i16, [diamond[0], diamond[1]]),
                ] {
                    let slot = edge_byte % 12;
                    let group = (edge_byte / 12) as usize;
                    if edge_byte == 0 || slot == 0 || group >= 18 {
                        continue;
                    }
                    if let Some(cs) = self.resolve_wall_edge_sprite(&ctx, edge_byte, fox_dir) {
                        if !cs.is_one_by_one {
                            push_cached_sprite_op(
                                &mut fg_ops,
                                tile.tile_top_left,
                                (anchor_dx, -8),
                                scale,
                                &cs,
                            );
                        }
                    } else if (slot >= 4 || slot == 1 || slot == 3)
                        && group > 1
                        && !self
                            .wall_caches
                            .get(&(group + 1))
                            .is_some_and(|c| c.is_loaded_for_mode(false))
                    {
                        // Missing wall_<n>.fox in dream/assets: skip silently instead of drawing wireframe lines.
                    }
                }
            }

            let portal_state = portal_states.get(&(tile.x as u16, tile.y as u16)).copied();
            let dream_overrides_item = tile.item_id == 128
                && self
                    .items_cache
                    .dream_archive
                    .as_ref()
                    .is_some_and(|f| f.resolve_shape(tile.item_id, 0).is_some());
            let eff_item = if tile.item_id > 0 {
                if dream_overrides_item {
                    tile.item_id
                } else {
                    resolve_item_variant(tile.item_id, tile.x, tile.y)
                }
            } else {
                0
            };
            let item_res = if eff_item > 0 || (self.world.is_modern && portal_state.is_some()) {
                self.resolve_item_layers(&ctx, eff_item, portal_state, anim_ms)
            } else {
                None
            };
            if let Some(ref res) = item_res {
                any_animated |= res.is_animated;
            } else if tile.item_id > 0 {
                let [r, g, b] = self
                    .palette
                    .rgb(((tile.item_id.wrapping_mul(29)) % 220 + 20) as u8);
                let pos = tile.center
                    + Vec2::new(tile.floor_elev.0 as f32, tile.floor_elev.1 as f32) * scale;
                fg_ops.push(MapDrawOp::CircleFilled(
                    pos,
                    (tile_h * 0.22).max(2.5),
                    Color32::from_rgb(r, g, b),
                ));
            }

            let eff_res = if tile.effect_id > 0 {
                self.resolve_effect_layers(&ctx, tile.effect_id, anim_ms)
            } else {
                None
            };
            if let Some(ref res) = eff_res {
                any_animated |= res.is_animated;
            }

            let item_elev = item_res.as_ref().map_or((0, 0), |r| r.elev);
            let fx_elev = eff_res.as_ref().map_or((0, 0), |r| r.elev);
            let fx_has_elev = eff_res.is_some() && fx_elev != (0, 0);
            let fx_shift = if fx_has_elev { tile.floor_elev } else { (0, 0) };
            let av_elev = if fx_has_elev {
                (
                    tile.floor_elev.0.saturating_add(fx_elev.0),
                    tile.floor_elev.1.saturating_add(fx_elev.1),
                )
            } else {
                (
                    tile.floor_elev.0.saturating_add(item_elev.0),
                    tile.floor_elev.1.saturating_add(item_elev.1),
                )
            };
            if av_elev != (0, 0) {
                tile_elevs.insert((tile.x, tile.y), av_elev);
            }

            let fx_pass = eff_res.as_ref().map_or(1, |r| r.effect_pass);
            // Pass 2a (slots 0 & 1 behind avatar): effect before item if fx_pass == 1.
            if fx_pass == 1 {
                if let Some(ref fx) = eff_res {
                    let fx_alpha = ((fx.alpha as u16 * 75) / 255) as u8;
                    collect_shape_slot_ops(
                        &fx.layers,
                        fx_alpha,
                        0..2,
                        tile.tile_top_left,
                        scale,
                        fx_shift,
                        &mut shadow_ops,
                        &mut fg_ops,
                    );
                }
                if let Some(ref it) = item_res {
                    collect_shape_slot_ops(
                        &it.layers,
                        it.alpha,
                        0..2,
                        tile.tile_top_left,
                        scale,
                        tile.floor_elev,
                        &mut shadow_ops,
                        &mut fg_ops,
                    );
                }
            } else {
                if let Some(ref it) = item_res {
                    collect_shape_slot_ops(
                        &it.layers,
                        it.alpha,
                        0..2,
                        tile.tile_top_left,
                        scale,
                        tile.floor_elev,
                        &mut shadow_ops,
                        &mut fg_ops,
                    );
                }
                if let Some(ref fx) = eff_res {
                    let fx_alpha = ((fx.alpha as u16 * 75) / 255) as u8;
                    collect_shape_slot_ops(
                        &fx.layers,
                        fx_alpha,
                        0..2,
                        tile.tile_top_left,
                        scale,
                        fx_shift,
                        &mut shadow_ops,
                        &mut fg_ops,
                    );
                }
            }

            let tile_hidden = self.world.map.as_ref().is_some_and(|m| {
                let tr = m.region_at(tile.x, tile.y);
                tr >= thresh && cur_region >= thresh && tr != cur_region
            });
            while av_idx < sorted_avatars.len() {
                let av = &sorted_avatars[av_idx];
                let ay = av.y as usize;
                let ax = av.x as usize;
                if (ay, ax) != (tile.y, tile.x) {
                    break;
                }
                if !tile_hidden {
                    let pos = tile.center + Vec2::new(av_elev.0 as f32, av_elev.1 as f32) * scale;
                    if cull_rect.contains(pos) {
                        self.collect_avatar_ops(
                            &ctx,
                            pos,
                            tile_h,
                            scale,
                            anim_ms,
                            av,
                            &mut hitboxes,
                            &mut name_badges,
                            &mut shadow_ops,
                            &mut fg_ops,
                            &mut any_animated,
                        );
                    }
                }
                av_idx += 1;
            }

            // Pass 2b (slots 2 & 3 in front of avatar): effect after item if fx_pass == 2.
            if fx_pass == 2 {
                if let Some(ref fx) = eff_res {
                    let fx_alpha = ((fx.alpha as u16 * 75) / 255) as u8;
                    collect_shape_slot_ops(
                        &fx.layers,
                        fx_alpha,
                        2..4,
                        tile.tile_top_left,
                        scale,
                        fx_shift,
                        &mut shadow_ops,
                        &mut fg_ops,
                    );
                }
                if let Some(ref it) = item_res {
                    collect_shape_slot_ops(
                        &it.layers,
                        it.alpha,
                        2..4,
                        tile.tile_top_left,
                        scale,
                        tile.floor_elev,
                        &mut shadow_ops,
                        &mut fg_ops,
                    );
                }
            } else {
                if let Some(ref it) = item_res {
                    collect_shape_slot_ops(
                        &it.layers,
                        it.alpha,
                        2..4,
                        tile.tile_top_left,
                        scale,
                        tile.floor_elev,
                        &mut shadow_ops,
                        &mut fg_ops,
                    );
                }
                if let Some(ref fx) = eff_res {
                    let fx_alpha = ((fx.alpha as u16 * 75) / 255) as u8;
                    collect_shape_slot_ops(
                        &fx.layers,
                        fx_alpha,
                        2..4,
                        tile.tile_top_left,
                        scale,
                        fx_shift,
                        &mut shadow_ops,
                        &mut fg_ops,
                    );
                }
            }
        }

        while av_idx < sorted_avatars.len() {
            let av = &sorted_avatars[av_idx];
            let ax = av.x as usize;
            let ay = av.y as usize;
            let hidden = self.world.map.as_ref().is_some_and(|m| {
                ax < m.width && ay < m.height && {
                    let tr = m.region_at(ax, ay);
                    tr >= thresh && cur_region >= thresh && tr != cur_region
                }
            });
            if !hidden {
                let av_elev = if let Some(&e) = tile_elevs.get(&(ax, ay)) {
                    e
                } else {
                    self.resolve_map_tile_elev(&ctx, cur_region, ax, ay, anim_ms)
                };
                let pos = origin
                    + tile_to_screen(av.x as f32, av.y as f32, tile_w, tile_h)
                    + Vec2::new(av_elev.0 as f32, av_elev.1 as f32) * scale;
                if cull_rect.contains(pos) {
                    self.collect_avatar_ops(
                        &ctx,
                        pos,
                        tile_h,
                        scale,
                        anim_ms,
                        av,
                        &mut hitboxes,
                        &mut name_badges,
                        &mut shadow_ops,
                        &mut fg_ops,
                        &mut any_animated,
                    );
                }
            }
            av_idx += 1;
        }

        emit_map_draw_ops(&painter, &shadow_ops);
        emit_map_draw_ops(&painter, &fg_ops);

        // Pass 3: floating dream portal titles & subtitles (`]s`), matching `tl` & `Br` (`L1951`, `L1456`).
        let dream_labels: Vec<furcadia_client::DreamLabel> =
            self.world.dream_labels.values().cloned().collect();
        let label_font = FontId::proportional((12.0 * self.zoom).clamp(10.0, 18.0).round());
        let label_max_w = 140.0 * self.zoom.max(0.85);
        for dl in dream_labels {
            let (lx, ly) = (dl.x as usize, dl.y as usize);
            let visible = if let Some(map) = &self.world.map {
                if lx >= map.width || ly >= map.height {
                    false
                } else {
                    let k = map.region_at(lx, ly);
                    k == cur_region || (cur_region < thresh && k < thresh)
                }
            } else {
                true
            };
            if !visible {
                continue;
            }
            let fe = if let Some(&e) = floor_elevs.get(&(lx, ly)) {
                e
            } else {
                self.resolve_map_floor_elev(&ctx, cur_region, lx, ly)
            };
            let anchor = origin
                + tile_to_screen(dl.x as f32, dl.y as f32, tile_w, tile_h)
                + Vec2::new(fe.0 as f32, -33.0 + fe.1 as f32) * scale;
            if !rect.expand(120.0).contains(anchor) {
                continue;
            }
            draw_dream_label_badge(
                &painter,
                anchor,
                &dl.title,
                &dl.subtitle,
                &label_font,
                label_max_w,
                &self.smiley_textures,
                scale,
            );
        }

        if any_animated && self.anim_mode != "Off" {
            let delay_ms = if self.anim_mode == "Half" { 160 } else { 80 };
            ctx.request_repaint_after(std::time::Duration::from_millis(delay_ms));
        }

        let badge_font = FontId::proportional((11.5 * self.zoom).clamp(9.0, 16.0).round());
        let prev_hover_uid = self.hovered_avatar_uid;
        let ctx_menu_uid = self.context_menu_avatar.as_ref().map(|c| c.uid);
        for badge in &name_badges {
            let is_highlighted =
                prev_hover_uid == Some(badge.uid) || ctx_menu_uid == Some(badge.uid);
            let show_this = self.show_avatar_names || is_highlighted;
            if !show_this {
                continue;
            }
            let label_pos = badge.pos - Vec2::new(0.0, tile_h * 1.25);
            if let Some(pill_rect) = draw_player_name_badge(
                &painter,
                rect,
                label_pos,
                &badge.name,
                badge.is_self,
                is_highlighted,
                &badge_font,
                &self.smiley_textures,
                scale,
            ) {
                if let Some(hb) = hitboxes.iter_mut().rev().find(|h| h.uid == badge.uid) {
                    hb.badge_rect = Some(pill_rect);
                }
            }
        }

        let hit_at = |p: Pos2| -> Option<&AvatarHitBox> {
            hitboxes
                .iter()
                .rev()
                .find(|h| h.contains_badge(p))
                .or_else(|| hitboxes.iter().rev().find(|h| h.contains_body(p)))
        };

        let new_hover_uid = response.hover_pos().and_then(|p| hit_at(p).map(|h| h.uid));
        if new_hover_uid != self.hovered_avatar_uid {
            self.hovered_avatar_uid = new_hover_uid;
            ctx.request_repaint();
        }
        if self.hovered_avatar_uid.is_some() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        if ui.input(|i| i.modifiers.shift) {
            if let Some(hp) = response.hover_pos() {
                if let Some(t) = tiles_to_draw.iter().min_by(|a, b| {
                    let da = ((hp.x - a.center.x) / (tile_w * 0.5)).abs()
                        + ((hp.y - a.center.y) / (tile_h * 0.5)).abs();
                    let db = ((hp.x - b.center.x) / (tile_w * 0.5)).abs()
                        + ((hp.y - b.center.y) / (tile_h * 0.5)).abs();
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    let (raw_f, raw_i, raw_w, raw_r, raw_l, raw_a) =
                        self.world.map.as_ref().map_or((0, 0, 0, 0, 0, 0), |m| {
                            (
                                m.floor_at(t.x, t.y),
                                m.item_at(t.x, t.y),
                                m.wall_at(t.x, t.y),
                                m.region_at(t.x, t.y),
                                m.lighting_at(t.x, t.y),
                                m.ambience_at(t.x, t.y),
                            )
                        });
                    egui::show_tooltip_at_pointer(
                        &ctx,
                        ui.layer_id(),
                        egui::Id::new("tile_debug_tooltip"),
                        |ui| {
                            ui.label(format!(
                                "Tile ({}, {})
Floor: {} (raw {})
Item: {} (raw {})
Wall NW: {} (tex={}, v={}) | NE: {} (tex={}, v={}) [raw 0x{:04x}]
Upper NW: {} | NE: {}
Region: {} | Effect: {}
Light: {} | Amb: {} | Elev: {:?}",
                                t.x, t.y,
                                t.floor_id, raw_f,
                                t.item_id, raw_i,
                                t.wall_nw, t.wall_nw / 12, t.wall_nw % 12,
                                t.wall_ne, t.wall_ne / 12, t.wall_ne % 12,
                                raw_w,
                                t.upper_nw, t.upper_ne,
                                raw_r, t.effect_id,
                                raw_l, raw_a, t.floor_elev
                            ));
                        },
                    );
                }
            }
        }

        if response.secondary_clicked() {
            if let Some(cp) = response.interact_pointer_pos() {
                if let Some(hb) = hit_at(cp) {
                    self.context_menu_avatar = Some(ContextMenuTarget {
                        uid: hb.uid,
                        display_name: hb.display_name.clone(),
                        short_name: hb.short_name.clone(),
                        is_self: hb.is_self,
                        anchor_pos: cp,
                        opened_this_frame: true,
                    });
                } else {
                    self.context_menu_avatar = None;
                }
            } else {
                self.context_menu_avatar = None;
            }
        } else if response.clicked() {
            if self.context_menu_avatar.is_some() {
                self.context_menu_avatar = None;
            } else if let Some(cp) = response.interact_pointer_pos() {
                if let Some((uid, short_name, display_name, _is_self)) = hit_at(cp).map(|hb| {
                    (
                        hb.uid,
                        hb.short_name.clone(),
                        hb.display_name.clone(),
                        hb.is_self,
                    )
                }) {
                    let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                    if ctrl_held {
                        self.open_whisper_tab(&display_name, true);
                    } else {
                        if let Some(av) = self.world.avatars.get(&uid).cloned() {
                            if let Some((eff_species, _)) = self.world.resolved_avatar_species_gender(&av) {
                                let (file_id, _) = self.resolve_species_spec(eff_species as usize);
                                let loaded = self
                                    .species_caches
                                    .get(&file_id)
                                    .is_some_and(FoxSpriteCache::is_loaded);
                                if !loaded {
                                    self.pending_species_downloads.remove(&file_id);
                                    self.ensure_species_cache(file_id);
                                }
                            }
                            let base_file_id = self.resolve_species_spec(av.appearance.species_id as usize).0;
                            let base_loaded = self
                                .species_caches
                                .get(&base_file_id)
                                .is_some_and(FoxSpriteCache::is_loaded);
                            if !base_loaded {
                                self.pending_species_downloads.remove(&base_file_id);
                                self.ensure_species_cache(base_file_id);
                            }
                        }
                        if let Some(&pid) = self.player_portrait_ids.get(&shortname_key(&short_name)) {
                            if matches!(self.custom_portrait_rgba.get(&pid), Some(None)) {
                                self.custom_portrait_rgba.remove(&pid);
                                self.pending_portrait_downloads.remove(&pid);
                            }
                            if !self.custom_portrait_rgba.contains_key(&pid)
                                && self.pending_portrait_downloads.insert(pid)
                            {
                                let _ = self.to_net.send(UiToNet::DownloadPortrait { portrait_id: pid });
                            }
                        }
                        if self.is_connected {
                            self.request_player_look(&short_name);
                        }
                    }
                } else if self.is_connected {
                    let self_screen = self
                        .world
                        .self_pos
                        .map(|(sx, sy)| {
                            origin
                                + tile_to_screen(sx as f32, sy as f32, tile_w, tile_h)
                                + Vec2::new(self_elev.0 as f32, self_elev.1 as f32) * scale
                        })
                        .unwrap_or_else(|| rect.center());
                    let d = cp - self_screen;
                    let dir = match (d.x < 0.0, d.y < 0.0) {
                        (true, true) => 7,
                        (false, true) => 9,
                        (true, false) => 1,
                        (false, false) => 3,
                    };
                    let _ = self.to_net.send(UiToNet::Send(ClientCommand::Move(dir)));
                }
            }
        }

        if let Some((sx, sy)) = self.world.self_pos {
            let already_drawn = self
                .world
                .self_uid
                .is_some_and(|uid| self.world.avatars.contains_key(&uid));
            if !already_drawn {
                let pos = origin
                    + tile_to_screen(sx as f32, sy as f32, tile_w, tile_h)
                    + Vec2::new(self_elev.0 as f32, self_elev.1 as f32) * scale;
                painter.circle_filled(
                    pos - Vec2::new(0.0, tile_h * 0.3),
                    7.5 * self.zoom,
                    Color32::from_rgb(90, 220, 140),
                );
                if let Some(name) = &self.world.self_name {
                    painter.text(
                        pos - Vec2::new(0.0, tile_h * 0.75),
                        egui::Align2::CENTER_BOTTOM,
                        name,
                        FontId::proportional((12.0 * self.zoom).clamp(9.0, 18.0).round()),
                        Color32::WHITE,
                    );
                }
            }
        }

        self.last_visible_avatar_count = hitboxes.len();

        if let Some(ref dream_name) = self.pending_dream_name {
            let overlay_painter = ui.painter_at(rect);
            overlay_painter.rect_filled(
                rect,
                0.0,
                Color32::from_rgba_unmultiplied(12, 8, 24, 185),
            );
            let card_w = (rect.width() - 32.0).clamp(240.0, 380.0);
            let card_h = 128.0_f32;
            let card_rect = Rect::from_center_size(rect.center(), Vec2::new(card_w, card_h));
            overlay_painter.rect_filled(card_rect, 8.0, CARD_PURPLE);
            overlay_painter.rect_stroke(card_rect, 8.0, Stroke::new(1.5_f32, GOLD));

            let display_dream = self
                .current_dream_url
                .as_deref()
                .map(|u| u.trim_start_matches("furc://").trim_matches('/'))
                .filter(|s| !s.is_empty())
                .unwrap_or(dream_name.as_str());
            overlay_painter.text(
                Pos2::new(card_rect.center().x, card_rect.min.y + 22.0),
                egui::Align2::CENTER_CENTER,
                format!("Downloading Dream: {display_dream}"),
                FontId::proportional(15.0),
                GOLD,
            );

            let stage_txt = if self.pending_dream_stage.is_empty() {
                "Downloading dream package..."
            } else {
                self.pending_dream_stage.as_str()
            };
            overlay_painter.text(
                Pos2::new(card_rect.center().x, card_rect.min.y + 44.0),
                egui::Align2::CENTER_CENTER,
                stage_txt,
                FontId::proportional(12.0),
                Color32::from_rgb(215, 210, 235),
            );

            let bar_w = (card_w - 40.0).max(120.0);
            let bar_rect = Rect::from_center_size(
                Pos2::new(card_rect.center().x, card_rect.min.y + 72.0),
                Vec2::new(bar_w, 14.0),
            );
            overlay_painter.rect_filled(bar_rect, 4.0, INPUT_PURPLE);
            overlay_painter.rect_stroke(bar_rect, 4.0, Stroke::new(1.0_f32, GOLD_DIM));

            let recv = self.pending_dream_received_bytes;
            let total_opt = self.pending_dream_total_bytes.filter(|&t| t > 0);
            if let Some(total) = total_opt {
                let frac = (recv as f32 / total as f32).clamp(0.0, 1.0);
                let fill_w = ((bar_rect.width() - 4.0) * frac).max(0.0);
                if fill_w > 0.5 {
                    let fill_rect = Rect::from_min_size(
                        Pos2::new(bar_rect.min.x + 2.0, bar_rect.min.y + 2.0),
                        Vec2::new(fill_w, bar_rect.height() - 4.0),
                    );
                    overlay_painter.rect_filled(fill_rect, 3.0, GOLD);
                }
            } else {
                let t = ui.input(|i| i.time) as f32;
                let seg_w = bar_rect.width() * 0.32;
                let travel = (bar_rect.width() - seg_w - 4.0).max(0.0);
                let phase = ((t * 1.6).sin() * 0.5) + 0.5;
                let x0 = bar_rect.min.x + 2.0 + travel * phase;
                let fill_rect = Rect::from_min_size(
                    Pos2::new(x0, bar_rect.min.y + 2.0),
                    Vec2::new(seg_w, bar_rect.height() - 4.0),
                );
                overlay_painter.rect_filled(fill_rect, 3.0, GOLD);
                ctx.request_repaint_after(Duration::from_millis(50));
            }

            let progress_label = match total_opt {
                Some(total) => {
                    let pct = ((recv as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
                    format!(
                        "{} / {} ({pct:.0}%)",
                        format_bytes(recv),
                        format_bytes(total)
                    )
                }
                None if recv > 0 => format!("{} received", format_bytes(recv)),
                None => "Waiting for server response...".to_owned(),
            };
            overlay_painter.text(
                Pos2::new(card_rect.center().x, card_rect.min.y + 102.0),
                egui::Align2::CENTER_CENTER,
                progress_label,
                FontId::proportional(12.0),
                Color32::LIGHT_GRAY,
            );
        }

        self.render_player_context_menu(&ctx, ctx.screen_rect());
    }

    fn toggle_pounce_watch(&mut self, display_name: &str, short_name: &str) {
        if self.pounce.contains(display_name) {
            self.pounce.remove_watch(display_name);
            self.save_config();
            if self.is_connected {
                self.send_raw_cmd(&format!("pounce remuser {short_name}"));
            }
        } else {
            self.pounce.add_watch(display_name.to_owned());
            self.save_config();
            if self.is_connected {
                self.send_raw_cmd(&format!("pounce adduser {short_name}"));
            }
        }
    }

    fn execute_player_action(
        &mut self,
        display_name: &str,
        short_name: &str,
        action: PlayerMenuAction,
    ) {
        match action {
            PlayerMenuAction::Look => {
                if self.is_connected {
                    self.request_player_look(short_name);
                }
            }
            PlayerMenuAction::Whisper => {
                self.open_whisper_tab(display_name, true);
            }
            PlayerMenuAction::Join => self.send_raw_cmd(&format!("join {short_name}")),
            PlayerMenuAction::Summon => self.send_raw_cmd(&format!("summon {short_name}")),
            PlayerMenuAction::Follow => self.send_raw_cmd(&format!("follow {short_name}")),
            PlayerMenuAction::Lead => self.send_raw_cmd(&format!("lead {short_name}")),
            PlayerMenuAction::Cuddle => self.send_raw_cmd(&format!("cuddle {short_name}")),
            PlayerMenuAction::TogglePounce => self.toggle_pounce_watch(display_name, short_name),
            PlayerMenuAction::ToggleIgnore => self.send_raw_cmd(&format!("ignore {short_name}")),
            PlayerMenuAction::ShareEdit => self.send_raw_cmd(&format!("share {short_name}")),
            PlayerMenuAction::UnshareEdit => self.send_raw_cmd(&format!("unshare {short_name}")),
            PlayerMenuAction::Raw(cmd) => {
                self.send_raw_cmd(cmd);
                if cmd.contains("cookie") {
                    self.send_raw_cmd("info");
                }
            }
        }
    }

    fn handle_chat_click_action(&mut self, action: ChatClickAction) {
        match action {
            ChatClickAction::OpenPlayerMenu {
                display_name,
                short_name,
                pos,
            } => {
                let short = if short_name.trim().is_empty() {
                    wire_player_name(&display_name)
                } else {
                    wire_player_name(&short_name)
                };
                if short.is_empty() {
                    return;
                }
                let disp = if display_name.trim().is_empty() {
                    short.clone()
                } else {
                    display_name.trim().to_owned()
                };
                let self_disp = self
                    .world
                    .self_name
                    .as_deref()
                    .or_else(|| self.selected_character().map(|c| c.name.as_str()))
                    .unwrap_or("");
                let is_self = !self_disp.is_empty() && wire_player_name(self_disp) == short;
                let uid = self
                    .world
                    .avatars
                    .values()
                    .find(|a| wire_player_name(&a.name) == short)
                    .map(|a| a.uid)
                    .unwrap_or(0);
                self.context_menu_avatar = Some(ContextMenuTarget {
                    uid,
                    display_name: disp,
                    short_name: short.clone(),
                    is_self,
                    anchor_pos: pos,
                    opened_this_frame: true,
                });
                if self.is_connected {
                    self.request_player_look(&short);
                }
            }
            ChatClickAction::EnterDreamUrl(url) => {
                self.send_raw_cmd(&format!("fdl {url}"));
            }
            ChatClickAction::SendCommand(cmd) => {
                let trimmed = cmd.trim();
                if !trimmed.is_empty() {
                    self.send_raw_cmd(trimmed);
                }
            }
        }
    }

    fn render_player_context_menu(&mut self, ctx: &egui::Context, viewport: Rect) {
        let Some(target) = self.context_menu_avatar.clone() else {
            return;
        };
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.context_menu_avatar = None;
            return;
        }
        let in_pounce = !target.is_self && self.pounce.contains(&target.display_name);
        let menu_w = 172.0_f32;
        let menu_h = if target.is_self { 245.0_f32 } else { 300.0_f32 };
        let menu_pos = Pos2::new(
            target
                .anchor_pos
                .x
                .min((viewport.max.x - menu_w - 6.0).max(viewport.min.x + 4.0)),
            target
                .anchor_pos
                .y
                .min((viewport.max.y - menu_h - 6.0).max(viewport.min.y + 4.0)),
        );
        let mut chosen_action: Option<PlayerMenuAction> = None;
        let area_resp = egui::Area::new(egui::Id::new("player_ctx_menu"))
            .fixed_pos(menu_pos)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(148.0);
                    ui.label(
                        RichText::new(name_display_clean(&target.display_name))
                            .strong()
                            .color(self.ui_accent()),
                    );
                    ui.separator();
                    chosen_action =
                        render_player_menu_buttons(ui, target.is_self, in_pounce, false);
                });
            });
        if let Some(action) = chosen_action {
            self.execute_player_action(&target.display_name, &target.short_name, action);
            self.context_menu_avatar = None;
        } else if !target.opened_this_frame && area_resp.response.clicked_elsewhere() {
            self.context_menu_avatar = None;
        } else if let Some(cur) = self.context_menu_avatar.as_mut() {
            cur.opened_this_frame = false;
        }
    }

    fn render_looked_player_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(snapshot) = self.looked_player.clone() else {
            return;
        };
        let snapshot_name_key = shortname_key(&snapshot.info.name);
        let live_avatar = self
            .world
            .avatars
            .values()
            .find(|avatar| shortname_key(&avatar.name) == snapshot_name_key)
            .cloned();
        let appearance = live_avatar
            .as_ref()
            .map(|avatar| avatar.appearance.clone())
            .unwrap_or_else(|| snapshot.info.appearance.clone());
        let eff = {
            let sp = appearance.species_id;
            if sp == 1000 {
                None
            } else if (1001..=1080).contains(&sp) {
                let (mapped_sp, mapped_gender) =
                    self.world.local_species_remap[(sp - 1001) as usize];
                if mapped_sp == 0 {
                    Some((sp, appearance.gender))
                } else {
                    Some((mapped_sp, mapped_gender))
                }
            } else {
                Some((
                    if sp == 0 { 1 } else { sp },
                    appearance.gender,
                ))
            }
        };

        let (mut portrait_tex, mut badge_tex) = (snapshot.portrait_tex, snapshot.badge_tex);
        match eff {
            Some((eff_sp, eff_gender)) => {
                let cache_state = (eff_sp, eff_gender, appearance.color_key);
                if snapshot.resolved_for != Some(cache_state) {
                    let (file_id, obj_id) = self.resolve_species_spec(eff_sp as usize);
                    let cache_key = self.ensure_species_cache(file_id);
                    if let Some(cache) = self.species_caches.get_mut(&cache_key) {
                        portrait_tex = cache.get_or_load_portrait_tex(
                            ctx,
                            &self.palette,
                            &appearance,
                            obj_id,
                            eff_gender,
                            4,
                        );
                        badge_tex = cache.get_or_load_portrait_tex(
                            ctx,
                            &self.palette,
                            &appearance,
                            obj_id,
                            eff_gender,
                            35,
                        );
                    } else {
                        portrait_tex = None;
                        badge_tex = None;
                    }
                    if let Some(card) = self.looked_player.as_mut() {
                        card.portrait_tex = portrait_tex.clone();
                        card.badge_tex = badge_tex.clone();
                        card.resolved_for = Some(cache_state);
                    }
                }
            }
            None => {
                portrait_tex = None;
                badge_tex = None;
                if snapshot.resolved_for.is_some() {
                    if let Some(card) = self.looked_player.as_mut() {
                        card.portrait_tex = None;
                        card.badge_tex = None;
                        card.resolved_for = None;
                    }
                }
            }
        }

        let accent = self.ui_accent();
        let display_name = snapshot.info.name.replace('|', " ");
        let in_pounce = self.pounce.contains(&display_name);
        let mut close_card = false;
        let mut action: Option<PlayerMenuAction> = None;

        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(28, 30, 48))
            .stroke(Stroke::new(1.0_f32, accent.linear_multiply(0.75)))
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&display_name)
                            .strong()
                            .size(13.0)
                            .color(accent),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("×").on_hover_text("Dismiss").clicked() {
                            close_card = true;
                        }
                        if let Some(ref badge) = badge_tex {
                            let sz = badge.size_vec2();
                            let disp = if sz.x > 0.0 && sz.y > 0.0 {
                                let s = (20.0 / sz.y).min(1.0);
                                sz * s
                            } else {
                                Vec2::new(22.0, 19.0)
                            };
                            ui.image(egui::load::SizedTexture::new(badge.id(), disp));
                        }
                    });
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let port_size = Vec2::splat(self.portrait_size);
                    let custom_tex = snapshot
                        .portrait_id
                        .and_then(|pid| self.custom_portrait_texture(ctx, pid, &appearance));
                    if let Some(ref tex) = custom_tex.or(portrait_tex) {
                        let resp = ui.image(egui::load::SizedTexture::new(tex.id(), port_size));
                        if let Some(pid) = snapshot.portrait_id {
                            resp.on_hover_text(format!("Portrait #{pid}"));
                        }
                    } else {
                        let (r, p) = ui.allocate_painter(port_size, Sense::hover());
                        p.rect_filled(r.rect, 4.0, Color32::from_rgb(20, 22, 34));
                        p.rect_stroke(
                            r.rect,
                            4.0,
                            Stroke::new(1.0_f32, Color32::from_rgb(55, 60, 82)),
                        );
                        let hint = snapshot
                            .portrait_id
                            .map(|id| format!("#{id}"))
                            .unwrap_or_else(|| "Portrait".to_owned());
                        p.text(
                            r.rect.center(),
                            egui::Align2::CENTER_CENTER,
                            hint,
                            FontId::proportional(11.0),
                            Color32::GRAY,
                        );
                    }
                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            if ui.small_button("Whisper").clicked() {
                                action = Some(PlayerMenuAction::Whisper);
                            }
                            if ui.small_button("Join").clicked() {
                                action = Some(PlayerMenuAction::Join);
                            }
                            if ui.small_button("Summon").clicked() {
                                action = Some(PlayerMenuAction::Summon);
                            }
                            if ui.small_button("Follow").clicked() {
                                action = Some(PlayerMenuAction::Follow);
                            }
                            let pounce_lbl = if in_pounce { "-Pounce" } else { "+Pounce" };
                            if ui.small_button(pounce_lbl).clicked() {
                                action = Some(PlayerMenuAction::TogglePounce);
                            }
                            if self.is_connected
                                && !display_name.eq_ignore_ascii_case(&self.active_character_name())
                            {
                                let target_wire = if snapshot.short_name.trim().is_empty() {
                                    wire_player_name(&display_name)
                                } else {
                                    wire_player_name(&snapshot.short_name)
                                };
                                ui.menu_button("Cookie", |ui| {
                                    for (lbl, cmd, tip) in [
                                        (
                                            "Make Cookie",
                                            "make-cookie",
                                            "Bake a daily cookie for this player",
                                        ),
                                        (
                                            "Give Cookie",
                                            "give-cookie",
                                            "Give one of your held cookies",
                                        ),
                                        (
                                            "Make Secret Cookie",
                                            "make-secret",
                                            "Bake a daily cookie silently",
                                        ),
                                        (
                                            "Give Secret Cookie",
                                            "secret-cookie",
                                            "Give a held cookie silently",
                                        ),
                                    ] {
                                        if ui.button(lbl).on_hover_text(tip).clicked() {
                                            self.send_raw_cmd(&format!("{cmd} {target_wire}"));
                                            self.push_system_line(
                                                "emit",
                                                &format!("Cookie Sent `{cmd} {target_wire}`"),
                                            );
                                            ui.close_menu();
                                        }
                                    }
                                });
                                ui.menu_button("Gift", |ui| {
                                    for (lbl, cmd) in [
                                        ("Give Item", "give"),
                                        ("Rose", "give-rose"),
                                        ("Red Rose", "give-red-rose"),
                                        ("Black Rose", "give-black-rose"),
                                        ("Bouquet", "give-bouquet"),
                                        ("Bracelet", "give-bracelet"),
                                        ("Snug", "give-snug"),
                                        ("Candy Cane", "give-candy-cane"),
                                        ("Valentine", "give-valentine"),
                                        ("Flaming Sword", "give-flaming-sword"),
                                    ] {
                                        if ui.button(lbl).clicked() {
                                            self.send_raw_cmd(&format!("{cmd} {target_wire}"));
                                            self.push_system_line(
                                                "emit",
                                                &format!("Gift Sent `{cmd} {target_wire}`"),
                                            );
                                            ui.close_menu();
                                        }
                                    }
                                });
                                ui.menu_button("Trade", |ui| {
                                    fn send_trade_cmd(
                                        app: &mut FurcadiaApp,
                                        ui: &mut egui::Ui,
                                        cmd: &str,
                                    ) {
                                        app.send_raw_cmd(cmd);
                                        app.push_system_line(
                                            "emit",
                                            &format!("Trade Sent `{cmd}`"),
                                        );
                                        ui.close_menu();
                                    }
                                    fn trade_action_button(
                                        app: &mut FurcadiaApp,
                                        ui: &mut egui::Ui,
                                        label: impl Into<egui::WidgetText>,
                                        tip: &str,
                                        cmd: &str,
                                    ) {
                                        if ui.button(label).on_hover_text(tip).clicked() {
                                            send_trade_cmd(app, ui, cmd);
                                        }
                                    }

                                    ui.set_min_width(240.0);
                                    trade_action_button(
                                        self,
                                        ui,
                                        "Start Trade",
                                        &format!(
                                            "Start or join a secure trade with {display_name} (`trade-start {target_wire}`)"
                                        ),
                                        &format!("trade-start {target_wire}"),
                                    );
                                    trade_action_button(
                                        self,
                                        ui,
                                        "Trade Status",
                                        "Show items currently in the active trade (`trade-status`)",
                                        "trade-status",
                                    );
                                    ui.separator();
                                    ui.menu_button("Add Common Item", |ui| {
                                        ui.set_min_width(185.0);
                                        egui::ScrollArea::vertical()
                                            .id_salt("trade_common_items_scroll")
                                            .max_height(320.0)
                                            .show(ui, |ui| {
                                                for (idx, (group_name, items)) in
                                                    COMMON_TRADE_ITEM_GROUPS.iter().enumerate()
                                                {
                                                    if idx > 0 {
                                                        ui.separator();
                                                    }
                                                    ui.label(
                                                        RichText::new(*group_name)
                                                            .size(11.0)
                                                            .strong()
                                                            .color(Color32::GRAY),
                                                    );
                                                    for (label, keyword) in *items {
                                                        let cmd = format!("trade-add {keyword}");
                                                        trade_action_button(
                                                            self,
                                                            ui,
                                                            *label,
                                                            &format!("Add {label} (`{cmd}`)"),
                                                            &cmd,
                                                        );
                                                    }
                                                }
                                                ui.separator();
                                                ui.label(
                                                    RichText::new("DragonScales (enter amount)")
                                                        .size(11.0)
                                                        .strong()
                                                        .color(Color32::GRAY),
                                                );
                                                for (label, prefix) in [
                                                    ("Golden DragonScales (GD)", "GD "),
                                                    ("Silver DragonScales (SD)", "SD "),
                                                    ("Copper DragonScales (CD)", "CD "),
                                                ] {
                                                    if ui
                                                        .button(label)
                                                        .on_hover_text(format!(
                                                            "Prefill `{prefix}` in the Item box below, then type an amount"
                                                        ))
                                                        .clicked()
                                                    {
                                                        self.trade_item_input = prefix.to_owned();
                                                    }
                                                }
                                            });
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add_sized(
                                            [46.0, 18.0],
                                            egui::Label::new(RichText::new("Item:").size(12.0)),
                                        );
                                        let item_resp = ui.add(
                                            egui::TextEdit::singleline(&mut self.trade_item_input)
                                                .hint_text("portrait, GD 50...")
                                                .desired_width(118.0),
                                        );
                                        let item_cmd_add =
                                            build_trade_command("trade-add", &self.trade_item_input);
                                        let item_cmd_rem = build_trade_command(
                                            "trade-remove",
                                            &self.trade_item_input,
                                        );
                                        let enter_add = item_resp.lost_focus()
                                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                        let clicked_add = ui
                                            .add_enabled(
                                                item_cmd_add.is_some(),
                                                egui::Button::new("+Add").small(),
                                            )
                                            .on_hover_text(
                                                "Add item or DragonScales (`trade-add <item>`)",
                                            )
                                            .clicked();
                                        let clicked_rem = ui
                                            .add_enabled(
                                                item_cmd_rem.is_some(),
                                                egui::Button::new("-Rem").small(),
                                            )
                                            .on_hover_text(
                                                "Remove item from trade (`trade-remove <item>`)",
                                            )
                                            .clicked();
                                        let chosen = if clicked_add || enter_add {
                                            item_cmd_add
                                        } else if clicked_rem {
                                            item_cmd_rem
                                        } else {
                                            None
                                        };
                                        if let Some(cmd) = chosen {
                                            send_trade_cmd(self, ui, &cmd);
                                            self.trade_item_input.clear();
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add_sized(
                                            [46.0, 18.0],
                                            egui::Label::new(RichText::new("Reason:").size(12.0)),
                                        );
                                        let reason_resp = ui.add(
                                            egui::TextEdit::singleline(
                                                &mut self.trade_reason_input,
                                            )
                                            .hint_text("reason for trade...")
                                            .desired_width(150.0),
                                        );
                                        let reason_cmd = build_trade_command(
                                            "trade-reason",
                                            &self.trade_reason_input,
                                        );
                                        let enter_reason = reason_resp.lost_focus()
                                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                        let clicked_reason = ui
                                            .add_enabled(
                                                reason_cmd.is_some(),
                                                egui::Button::new("Set").small(),
                                            )
                                            .on_hover_text(
                                                "Log a trade reason (`trade-reason <text>`)",
                                            )
                                            .clicked();
                                        if (clicked_reason || enter_reason)
                                            && reason_cmd.is_some()
                                        {
                                            if let Some(cmd) = reason_cmd {
                                                send_trade_cmd(self, ui, &cmd);
                                                self.trade_reason_input.clear();
                                            }
                                        }
                                    });
                                    ui.separator();
                                    trade_action_button(
                                        self,
                                        ui,
                                        RichText::new("Accept Trade")
                                            .color(Color32::from_rgb(130, 225, 145)),
                                        "Finalize and accept the trade offer — cannot be undone once both players accept (`trade-accept`)",
                                        "trade-accept",
                                    );
                                    trade_action_button(
                                        self,
                                        ui,
                                        RichText::new("Reject Trade")
                                            .color(Color32::from_rgb(240, 195, 115)),
                                        "Reject the current trade offer (`trade-reject`)",
                                        "trade-reject",
                                    );
                                    trade_action_button(
                                        self,
                                        ui,
                                        RichText::new("End Trade")
                                            .color(Color32::from_rgb(235, 130, 130)),
                                        "Cancel and end the active trade session (`trade-end`)",
                                        "trade-end",
                                    );
                                });
                            }
                        });
                    });
                });
            });
        ui.add_space(6.0);

        if let Some(act) = action {
            self.execute_player_action(&display_name, &snapshot.short_name, act);
        }
        if close_card {
            self.looked_player = None;
            self.recent_portrait_target = None;
            if self.active_chat_tab.is_none() {
                self.main_looked_player = None;
            }
        }
    }

    /// Computes active floor sprite elevation `(dx, dy)` for `(tx, ty)` under region rules.
    fn resolve_map_floor_elev(
        &mut self,
        ctx: &egui::Context,
        cur_region: u16,
        tx: usize,
        ty: usize,
    ) -> (i16, i16) {
        let Some((tile_region, raw_f)) = self.world.map.as_ref().and_then(|map| {
            if tx < map.width && ty < map.height {
                Some((map.region_at(tx, ty), map.floor_at(tx, ty)))
            } else {
                None
            }
        }) else {
            return (0, 0);
        };
        let eff_f =
            self.world
                .resolve_tile_layer(cur_region, tile_region, RegionLayer::Floor, raw_f);
        self.resolve_floor_sprite(ctx, eff_f, tx, ty)
            .map_or((0, 0), |cs| cs.elev)
    }

    /// Computes total `(floor_elev + item_or_effect_elev)` for `(tx, ty)` using active region rules.
    fn resolve_map_tile_elev(
        &mut self,
        ctx: &egui::Context,
        cur_region: u16,
        tx: usize,
        ty: usize,
        anim_ms: u64,
    ) -> (i16, i16) {
        let Some((tile_region, raw_f, raw_i, raw_e)) = self.world.map.as_ref().and_then(|map| {
            if tx < map.width && ty < map.height {
                Some((
                    map.region_at(tx, ty),
                    map.floor_at(tx, ty),
                    map.item_at(tx, ty),
                    map.effect_at(tx, ty),
                ))
            } else {
                None
            }
        }) else {
            return (0, 0);
        };
        let eff_f =
            self.world
                .resolve_tile_layer(cur_region, tile_region, RegionLayer::Floor, raw_f);
        let eff_i = resolve_item_variant(
            self.world
                .resolve_tile_layer(cur_region, tile_region, RegionLayer::Item, raw_i),
            tx,
            ty,
        );
        let eff_e =
            self.world
                .resolve_tile_layer(cur_region, tile_region, RegionLayer::Effect, raw_e);
        let portal_state = self
            .world
            .dream_labels
            .get(&(tx as u16, ty as u16))
            .map(|dl| dl.state_flag);
        let f_elev = self
            .resolve_floor_sprite(ctx, eff_f, tx, ty)
            .map_or((0, 0), |cs| cs.elev);
        let fx_elev = if eff_e > 0 {
            self.resolve_effect_layers(ctx, eff_e, anim_ms)
                .map_or((0, 0), |r| r.elev)
        } else {
            (0, 0)
        };
        let sub_elev = if fx_elev != (0, 0) {
            fx_elev
        } else if eff_i > 0 || (self.world.is_modern && portal_state.is_some()) {
            self.resolve_item_layers(ctx, eff_i, portal_state, anim_ms)
                .map_or((0, 0), |r| r.elev)
        } else {
            (0, 0)
        };
        (
            f_elev.0.saturating_add(sub_elev.0),
            f_elev.1.saturating_add(sub_elev.1),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_avatar_ops(
        &mut self,
        ctx: &egui::Context,
        pos: Pos2,
        tile_h: f32,
        scale: f32,
        anim_ms: u64,
        av: &Avatar,
        hitboxes: &mut Vec<AvatarHitBox>,
        name_badges: &mut Vec<NameBadge>,
        shadow_ops: &mut Vec<MapDrawOp>,
        fg_ops: &mut Vec<MapDrawOp>,
        any_animated: &mut bool,
    ) {
        if !av.visible {
            return;
        }
        let Some((eff_species_u16, eff_gender)) = self.world.resolved_avatar_species_gender(av)
        else {
            return;
        };
        let is_self = self.world.self_uid == Some(av.uid);
        let (file_id, obj_id) = self.resolve_species_spec(eff_species_u16 as usize);
        let cache_key = self.ensure_species_cache(file_id);

        let mut drew_sprite = false;
        let mut sprite_bounds: Option<Rect> = None;
        let mut resolved_layers = self.species_caches.get_mut(&cache_key).and_then(|cache| {
            cache.get_or_load_avatar_layers(
                ctx,
                &self.palette,
                &av.appearance,
                obj_id,
                eff_gender,
                av.dir,
                av.pose,
                anim_ms,
            )
        });
        if resolved_layers.is_none() {
            let fallback_spec = av.appearance.species_id as usize;
            let (fb_file, fb_obj) = self.resolve_species_spec(fallback_spec);
            let fb_key = self.ensure_species_cache(fb_file);
            resolved_layers = self.species_caches.get_mut(&fb_key).and_then(|cache| {
                cache.get_or_load_avatar_layers(
                    ctx,
                    &self.palette,
                    &av.appearance,
                    fb_obj,
                    eff_gender,
                    av.dir,
                    av.pose,
                    anim_ms,
                )
            });
            if resolved_layers.is_none() {
                let s1_key = self.ensure_species_cache(1);
                resolved_layers = self.species_caches.get_mut(&s1_key).and_then(|cache| {
                    cache.get_or_load_avatar_layers(
                        ctx,
                        &self.palette,
                        &av.appearance,
                        0,
                        eff_gender,
                        av.dir,
                        av.pose,
                        anim_ms,
                    )
                });
            }
        }
        if let Some((layers, animated)) = resolved_layers {
            *any_animated |= animated;
            let av_s = if (av.scale - 1.0).abs() > f32::EPSILON {
                av.scale.max(0.01)
            } else {
                1.0
            };
            let pivot = if (av_s - 1.0).abs() > f32::EPSILON {
                let mut min_x = f32::INFINITY;
                let mut max_x = f32::NEG_INFINITY;
                let mut max_y = f32::NEG_INFINITY;
                for (tex, total_off, category) in &layers {
                    if (*category & 0x40) != 0 {
                        continue;
                    }
                    let sz = tex.size_vec2();
                    if sz.x <= 1.0 && sz.y <= 1.0 {
                        continue;
                    }
                    let lx0 = total_off.0 as f32 - 33.0;
                    let ly0 = total_off.1 as f32 - 79.0;
                    min_x = min_x.min(lx0);
                    max_x = max_x.max(lx0 + sz.x);
                    max_y = max_y.max(ly0 + sz.y);
                }
                if min_x.is_finite() {
                    Vec2::new((min_x + max_x) * 0.5, max_y)
                } else {
                    Vec2::ZERO
                }
            } else {
                Vec2::ZERO
            };
            let av_off = Vec2::new(av.offset.0 as f32 * scale, av.offset.1 as f32 * scale);
            for (tex, total_off, category) in layers {
                let is_shadow = (category & 0x40) != 0;
                let local_off = Vec2::new(total_off.0 as f32 - 33.0, total_off.1 as f32 - 79.0);
                let scaled_off = pivot + (local_off - pivot) * av_s;
                let top_left = pos + scaled_off * scale + av_off;
                let size = tex.size_vec2() * (scale * av_s);
                let sprite_rect = Rect::from_min_size(top_left, size);
                let tint = if is_shadow {
                    let shadow_a = if let Some((_, _, _, alpha)) = av.gloaming {
                        let out_a = 255_u8.saturating_sub(alpha);
                        ((115_u16 * out_a as u16) / 255) as u8
                    } else {
                        115
                    };
                    Color32::from_rgba_unmultiplied(255, 255, 255, shadow_a)
                } else if let Some((r, g, b, alpha)) = av.gloaming {
                    let out_a = 255_u8.saturating_sub(alpha);
                    if (r, g, b) == (0, 0, 0) {
                        Color32::from_rgba_unmultiplied(255, 255, 255, out_a)
                    } else {
                        Color32::from_rgba_unmultiplied(r, g, b, out_a)
                    }
                } else {
                    Color32::WHITE
                };
                let op = MapDrawOp::Sprite {
                    tex,
                    rect: sprite_rect,
                    tint,
                };
                if is_shadow {
                    shadow_ops.push(op);
                } else {
                    fg_ops.push(op);
                    sprite_bounds = Some(match sprite_bounds {
                        Some(b) => b.union(sprite_rect),
                        None => sprite_rect,
                    });
                    drew_sprite = true;
                }
            }
        }

        let mut fallback_center = None;
        if !drew_sprite {
            let body_color = if is_self {
                Color32::from_rgb(90, 220, 140)
            } else {
                Color32::from_rgb(100, 175, 245)
            };
            let body_center = pos - Vec2::new(0.0, tile_h * 0.35);
            let head_center = pos - Vec2::new(0.0, tile_h * 0.85);
            fg_ops.push(MapDrawOp::CircleFilled(
                body_center,
                8.0 * self.zoom,
                body_color,
            ));
            fg_ops.push(MapDrawOp::CircleStroke(
                body_center,
                8.0 * self.zoom,
                Stroke::new(1.2_f32, Color32::BLACK),
            ));
            fg_ops.push(MapDrawOp::CircleFilled(
                head_center,
                5.5 * self.zoom,
                body_color,
            ));
            fg_ops.push(MapDrawOp::CircleStroke(
                head_center,
                5.5 * self.zoom,
                Stroke::new(1.2_f32, Color32::BLACK),
            ));
            fallback_center = Some(body_center);
        }

        let display_name = av.name.replace('|', " ");
        let short_name = wire_player_name(&av.name);
        hitboxes.push(AvatarHitBox {
            uid: av.uid,
            display_name: display_name.clone(),
            short_name,
            is_self,
            sprite_rect: sprite_bounds,
            badge_rect: None,
            fallback_center,
            fallback_radius: 16.0 * self.zoom,
        });
        name_badges.push(NameBadge {
            uid: av.uid,
            pos,
            name: display_name,
            is_self,
        });
    }
}

/// Deterministic tile variant PRNG matching `Furcadia.exe` `0x4243c0(x, y)`.
fn furc_tile_prng(x: usize, y: usize) -> u16 {
    let xi = x as i32;
    let yi = y as i32;
    let seed = 18_i32
        .wrapping_mul(xi)
        .wrapping_add(yi)
        .wrapping_add(yi >> 2) as u32;
    (((seed.wrapping_mul(214_013).wrapping_add(2_531_011)) >> 16) & 0x7fff) as u16
}

/// Deterministic floor variant PRNG matching the Furcadia web/native floor renderer (`FloorVariant`).
fn resolve_floor_variant(floor_id: u16, x: usize, y: usize) -> u16 {
    match floor_id {
        0 | 32 | 40 => floor_id.wrapping_add(furc_tile_prng(x, y) & 7),
        8 | 116 | 120 | 124 => floor_id.wrapping_add(furc_tile_prng(x, y) & 3),
        _ => floor_id,
    }
}

/// Deterministic tile variant PRNG matching `Furcadia.exe` `0x47bf80` -> `0x4243c0(x, y, 16) + 128`.
fn resolve_item_variant(item_id: u16, x: usize, y: usize) -> u16 {
    if item_id == 128 {
        128 + (furc_tile_prng(x, y) & 0x0f)
    } else {
        item_id
    }
}

fn cache_has_portal_shapes(cache: &FoxSpriteCache, obj_id: u16) -> bool {
    [cache.dream_archive.as_ref(), cache.default_archive.as_ref()]
        .into_iter()
        .flatten()
        .any(|fox| fox.has_portal_shapes(obj_id))
}

/// Resolves portal layers from `cache`, preferring active `p=29` highest `s <= flag` when open (`flag >= 0`),
/// or closed `p=28 s=0` otherwise, matching `Furcadia.exe` `0x435950`.
fn resolve_portal_layers_from(
    cache: &mut FoxSpriteCache,
    ctx: &egui::Context,
    palette: &Palette,
    pid: u16,
    portal_state: Option<i32>,
    anim_ms: u64,
) -> Option<ResolvedShapeLayers> {
    let pick_best_state = |cache: &FoxSpriteCache, obj_id: u16, purpose: u8, cap: u8| -> Option<u8> {
        for maybe_fox in [cache.dream_archive.as_ref(), cache.default_archive.as_ref()] {
            let Some(fox) = maybe_fox else {
                continue;
            };
            let Some(shapes) = fox.object_avatar_shapes.get(&obj_id) else {
                continue;
            };
            if let Some(best) = shapes
                .iter()
                .filter(|s| {
                    s.purpose == purpose
                        && s.state <= cap
                        && (!s.frames.is_empty() || !s.layers.is_empty())
                })
                .map(|s| s.state)
                .max()
            {
                return Some(best);
            }
        }
        None
    };

    if let Some(flag) = portal_state.filter(|&f| f >= 0) {
        let slot = flag as usize;
        let prefix = cache.prefix.clone();
        for (is_dream, maybe_fox) in [
            (true, cache.dream_archive.as_mut()),
            (false, cache.default_archive.as_mut()),
        ] {
            let Some(fox) = maybe_fox else {
                continue;
            };
            let Some(shape) = fox.resolve_portal_swirl_shape(pid, slot).cloned() else {
                continue;
            };
            if let Some(res) = FoxSpriteCache::resolve_shape_slots(
                fox,
                if is_dream { ARCHIVE_DREAM } else { ARCHIVE_DEFAULT },
                &prefix,
                &mut cache.image_textures,
                ctx,
                palette,
                &shape,
                anim_ms,
            ) {
                return Some(res);
            }
        }
        let cap = flag.clamp(0, 255) as u8;
        let best_state = pick_best_state(cache, pid, 29, cap);
        if best_state.is_some() {
            if let res @ Some(_) =
                cache.get_or_load_ps_layers(ctx, palette, pid, 29, best_state, 0, anim_ms)
            {
                return res;
            }
        }
        if !cache_has_portal_shapes(cache, pid) {
            if let res @ Some(_) = cache.get_or_load_shape_layers(ctx, palette, pid, 0, anim_ms) {
                return res;
            }
        }
        if pid == 0 {
            cache
                .get_or_load_ps_layers(ctx, palette, pid, 29, None, 0, anim_ms)
                .or_else(|| cache.get_or_load_ps_layers(ctx, palette, pid, 28, Some(0), 0, anim_ms))
                .or_else(|| cache.get_or_load_shape_layers(ctx, palette, pid, 0, anim_ms))
        } else {
            None
        }
    } else {
        cache
            .get_or_load_ps_layers(ctx, palette, pid, 28, Some(0), 0, anim_ms)
            .or_else(|| cache.get_or_load_ps_layers(ctx, palette, pid, 28, None, 0, anim_ms))
            .or_else(|| cache.get_or_load_shape_layers(ctx, palette, pid, 0, anim_ms))
    }
}

/// Computes `junction.fox` group (`group_or`) and 4-neighbor connection bitmask at `(x, y)`.
fn compute_junction_state(upper_nw: u16, upper_ne: u16, cur_nw: u16, cur_ne: u16) -> (usize, u8) {
    let split = |b: u16| -> (usize, u16) {
        if b > 0 && !b.is_multiple_of(12) && (b / 12) < 18 {
            ((b / 12) as usize, b % 12)
        } else {
            (0, 0)
        }
    };
    let (g_unw, s_unw) = split(upper_nw);
    let (g_une, s_une) = split(upper_ne);
    let (g_cnw, s_cnw) = split(cur_nw);
    let (g_cne, s_cne) = split(cur_ne);
    let group_or = g_unw | g_une | g_cnw | g_cne;
    let mut mask = 0u8;
    if matches!(s_unw, 1 | 2) {
        mask |= 0x1;
    }
    if matches!(s_une, 1 | 2) {
        mask |= 0x2;
    }
    if matches!(s_cnw, 1 | 2) {
        mask |= 0x4;
    }
    if matches!(s_cne, 1 | 2) {
        mask |= 0x8;
    }
    (group_or, mask)
}

enum MapDrawOp {
    Sprite {
        tex: TextureHandle,
        rect: Rect,
        tint: Color32,
    },
    CircleFilled(Pos2, f32, Color32),
    CircleStroke(Pos2, f32, Stroke),
}

fn push_cached_sprite_op(
    ops: &mut Vec<MapDrawOp>,
    tile_top_left: Pos2,
    anchor_off: (i16, i16),
    scale: f32,
    cs: &CachedSprite,
) {
    let top_left = tile_top_left
        + Vec2::new(
            (cs.offset.0 + anchor_off.0) as f32,
            (cs.offset.1 + anchor_off.1) as f32,
        ) * scale;
    let size = cs.tex.size_vec2() * scale;
    ops.push(MapDrawOp::Sprite {
        tex: cs.tex.clone(),
        rect: Rect::from_min_size(top_left, size),
        tint: Color32::WHITE,
    });
}

#[allow(clippy::too_many_arguments)]
fn collect_shape_slot_ops(
    layers: &[ResolvedLayerTex],
    shape_alpha: u8,
    slots: std::ops::Range<u8>,
    tile_top_left: Pos2,
    scale: f32,
    elev_shift: (i16, i16),
    shadow_ops: &mut Vec<MapDrawOp>,
    fg_ops: &mut Vec<MapDrawOp>,
) {
    for l in layers {
        if l.is_one_by_one || (l.category & 0x80) != 0 || !slots.contains(&l.slot) {
            continue;
        }
        let is_shadow = (l.category & 0x40) != 0;
        let top_left = tile_top_left
            + Vec2::new(
                (l.offset.0 + elev_shift.0) as f32,
                (l.offset.1 + elev_shift.1) as f32,
            ) * scale;
        let size = l.tex.size_vec2() * scale;
        let tint = if is_shadow {
            let shadow_a = ((115_u16 * shape_alpha as u16) / 255) as u8;
            Color32::from_rgba_unmultiplied(255, 255, 255, shadow_a)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, shape_alpha)
        };
        let op = MapDrawOp::Sprite {
            tex: l.tex.clone(),
            rect: Rect::from_min_size(top_left, size),
            tint,
        };
        if is_shadow {
            shadow_ops.push(op);
        } else {
            fg_ops.push(op);
        }
    }
}

fn emit_map_draw_ops(painter: &egui::Painter, ops: &[MapDrawOp]) {
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    for op in ops {
        match op {
            MapDrawOp::Sprite { tex, rect, tint } => {
                painter.image(tex.id(), *rect, uv, *tint);
            }
            MapDrawOp::CircleFilled(center, radius, color) => {
                painter.circle_filled(*center, *radius, *color);
            }
            MapDrawOp::CircleStroke(center, radius, stroke) => {
                painter.circle_stroke(*center, *radius, *stroke);
            }
        }
    }
}

#[derive(Clone)]
enum BadgeSeg {
    Text(String),
    Smiley { idx: usize, code: char },
}

fn smiley_index_for_code(ch: char) -> Option<usize> {
    match ch {
        'A'..='P' => Some((ch as usize) - ('A' as usize)),
        'Q'..='Z' => Some(16 + (ch as usize) - ('Q' as usize)),
        'a'..='y' => Some(26 + (ch as usize) - ('a' as usize)),
        '1'..='3' => Some(51 + (ch as usize) - ('1' as usize)),
        _ => None,
    }
}


fn emoji_fallback_for_code(code: char) -> Option<&'static str> {
    // Unicode stand-ins for plain-text labels (Pounce rows, whisper tabs,
    // context menu). Verified in-game: O = heart. The rest mirror the
    // smiley picker guesses; confirm with the picker and tune both tables.
    Some(match code {
        'A' => "\u{1f60a}", 'B' => "\u{1f601}", 'C' => "\u{1f609}",
        'D' => "\u{1f622}", 'E' => "\u{1f61b}", 'F' => "\u{1f62e}",
        'G' => "\u{1f620}", 'H' => "\u{1f60e}", 'I' => "\u{1f633}",
        'J' => "\u{1f607}", 'K' => "\u{1f608}", 'L' => "\u{2764}",
        'M' => "\u{1f494}", 'N' => "\u{1f339}", 'O' => "\u{2665}",
        'P' => "\u{1f3b5}", 'Q' => "\u{1f389}", 'R' => "\u{1f4a4}",
        'S' => "\u{1f914}", 'T' => "\u{1f602}", 'U' => "\u{1f618}",
        'V' => "\u{1f644}", 'W' => "\u{1f62c}", 'X' => "\u{1f634}",
        'Y' => "\u{1f44d}", 'Z' => "\u{1f44e}",
        _ => return None,
    })
}

/// Replaces `#S<x>` smiley codes with Unicode stand-ins for plain-text UI
/// (lists and menus render labels, not sprites; map badges and chat keep
/// drawing real sprites via parse_furc_badge_segments/split_inline_chat_codes).
fn name_display_clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        if c == '#' && it.peek() == Some(&'S') {
            let mut la = it.clone();
            la.next();
            if let Some(code) = la.next() {
                if let Some(fb) = emoji_fallback_for_code(code) {
                    out.push_str(fb);
                    it.next();
                    it.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

fn parse_furc_badge_segments(raw: &str) -> Vec<BadgeSeg> {
    let decoded = unescape_html(raw);
    let mut segs = Vec::new();
    let mut buf = String::new();
    let mut chars = decoded.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' && chars.peek() == Some(&'S') {
            let mut lookahead = chars.clone();
            lookahead.next();
            if let Some(code) = lookahead.next() {
                if let Some(idx) = smiley_index_for_code(code) {
                    if !buf.is_empty() {
                        segs.push(BadgeSeg::Text(std::mem::take(&mut buf)));
                    }
                    segs.push(BadgeSeg::Smiley { idx, code });
                    chars.next();
                    chars.next();
                    continue;
                }
            }
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        segs.push(BadgeSeg::Text(buf));
    }
    segs
}

fn push_badge_seg_merged(out: &mut Vec<BadgeSeg>, seg: BadgeSeg) {
    if let BadgeSeg::Text(ref t) = seg {
        if let Some(BadgeSeg::Text(prev)) = out.last_mut() {
            prev.push_str(t);
            return;
        }
    }
    out.push(seg);
}

fn render_inline_smiley_badge(ui: &mut egui::Ui, tex: &TextureHandle, font: &FontId) -> egui::Response {
    let sz = smiley_draw_size(tex, font);
    let pad = Vec2::new(3.5, 2.0);
    let (rect, resp) = ui.allocate_exact_size(sz + pad * 2.0, Sense::hover());
    ui.painter().rect_filled(rect, 4.0, Color32::from_rgb(225, 222, 232));
    ui.painter().rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0_f32, Color32::from_rgb(155, 148, 175)),
    );
    let img_rect = Rect::from_center_size(rect.center(), sz);
    ui.painter().image(
        tex.id(),
        img_rect,
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
    resp
}

fn smiley_draw_size(tex: &TextureHandle, font: &FontId) -> Vec2 {
    let sz = tex.size_vec2();
    let h = (font.size * 1.18).max(10.0);
    let w = if sz.y > 0.0 {
        sz.x * (h / sz.y)
    } else {
        h
    };
    Vec2::new(w, h)
}

fn measure_badge_line(
    painter: &egui::Painter,
    segs: &[BadgeSeg],
    font: &FontId,
    smileys: &[Option<TextureHandle>],
) -> Vec2 {
    let mut w = 0.0_f32;
    let mut h = font.size;
    for seg in segs {
        match seg {
            BadgeSeg::Text(s) => {
                let g = painter.layout_no_wrap(s.clone(), font.clone(), Color32::BLACK);
                w += g.size().x;
                h = h.max(g.size().y);
            }
            BadgeSeg::Smiley { idx, code } => {
                if let Some(tex) = smileys.get(*idx).and_then(|o| o.as_ref()) {
                    let sz = smiley_draw_size(tex, font);
                    w += sz.x;
                    h = h.max(sz.y);
                } else {
                    let g = painter.layout_no_wrap(format!("#S{code}"), font.clone(), Color32::BLACK);
                    w += g.size().x;
                    h = h.max(g.size().y);
                }
            }
        }
    }
    Vec2::new(w, h)
}

fn wrap_badge_segments(
    painter: &egui::Painter,
    segs: &[BadgeSeg],
    font: &FontId,
    max_w: f32,
    smileys: &[Option<TextureHandle>],
) -> Vec<Vec<BadgeSeg>> {
    let mut logical_lines: Vec<Vec<BadgeSeg>> = vec![Vec::new()];
    for seg in segs {
        match seg {
            BadgeSeg::Text(s) => {
                let normalized = s.replace('\r', "");
                let mut first = true;
                for part in normalized.split('\n') {
                    if !first {
                        logical_lines.push(Vec::new());
                    }
                    first = false;
                    if !part.is_empty() {
                        if let Some(cur) = logical_lines.last_mut() {
                            cur.push(BadgeSeg::Text(part.to_string()));
                        }
                    }
                }
            }
            BadgeSeg::Smiley { .. } => {
                if let Some(cur) = logical_lines.last_mut() {
                    cur.push(seg.clone());
                }
            }
        }
    }

    let mut wrapped = Vec::new();
    for raw_line in logical_lines {
        let mut tokens: Vec<(BadgeSeg, bool)> = Vec::new();
        for seg in raw_line {
            match seg {
                BadgeSeg::Text(s) => {
                    let mut chunk = String::new();
                    let mut in_space = None;
                    for c in s.chars() {
                        let sp = c == ' ' || c == '\t';
                        if in_space.is_some_and(|prev| prev != sp) && !chunk.is_empty() {
                            tokens.push((
                                BadgeSeg::Text(std::mem::take(&mut chunk)),
                                in_space.unwrap_or(false),
                            ));
                        }
                        in_space = Some(sp);
                        chunk.push(c);
                    }
                    if !chunk.is_empty() {
                        tokens.push((BadgeSeg::Text(chunk), in_space.unwrap_or(false)));
                    }
                }
                BadgeSeg::Smiley { .. } => tokens.push((seg, false)),
            }
        }

        let mut cur: Vec<BadgeSeg> = Vec::new();
        let mut cur_w = 0.0_f32;
        for (tok, is_space) in tokens {
            let tok_w = measure_badge_line(painter, std::slice::from_ref(&tok), font, smileys).x;
            if !cur.is_empty() && cur_w + tok_w > max_w {
                while let Some(BadgeSeg::Text(last)) = cur.last() {
                    if last.trim().is_empty() {
                        cur.pop();
                    } else {
                        break;
                    }
                }
                if !cur.is_empty() {
                    wrapped.push(std::mem::take(&mut cur));
                }
                cur_w = 0.0;
                if is_space {
                    continue;
                }
            }
            if cur.is_empty() && is_space {
                continue;
            }
            cur_w += tok_w;
            push_badge_seg_merged(&mut cur, tok);
        }
        while let Some(BadgeSeg::Text(last)) = cur.last() {
            if last.trim().is_empty() {
                cur.pop();
            } else {
                break;
            }
        }
        if !cur.is_empty() {
            wrapped.push(cur);
        }
    }
    wrapped
}

fn draw_halo_text(
    painter: &egui::Painter,
    top_left: Pos2,
    text: &str,
    font: &FontId,
    fill: Color32,
    halo: Color32,
) {
    let fg = painter.layout_no_wrap(text.to_string(), font.clone(), fill);
    let bg = painter.layout_no_wrap(text.to_string(), font.clone(), halo);
    for (dx, dy) in [
        (-1.0_f32, -1.0),
        (0.0, -1.0),
        (1.0, -1.0),
        (-1.0, 0.0),
        (1.0, 0.0),
        (-1.0, 1.0),
        (0.0, 1.0),
        (1.0, 1.0),
    ] {
        painter.galley(top_left + Vec2::new(dx, dy), bg.clone(), halo);
    }
    painter.galley(top_left, fg, fill);
}

#[allow(clippy::too_many_arguments)]
fn draw_badge_line(
    painter: &egui::Painter,
    start: Pos2,
    line_h: f32,
    segs: &[BadgeSeg],
    font: &FontId,
    fill: Color32,
    halo: Color32,
    smileys: &[Option<TextureHandle>],
) {
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    let mut x = start.x;
    for seg in segs {
        match seg {
            BadgeSeg::Text(s) => {
                let g = painter.layout_no_wrap(s.clone(), font.clone(), fill);
                let sz = g.size();
                let y = start.y + (line_h - sz.y) * 0.5;
                draw_halo_text(painter, Pos2::new(x, y), s, font, fill, halo);
                x += sz.x;
            }
            BadgeSeg::Smiley { idx, code } => {
                if let Some(tex) = smileys.get(*idx).and_then(|o| o.as_ref()) {
                    let sz = smiley_draw_size(tex, font);
                    let y = start.y + (line_h - sz.y) * 0.5;
                    painter.image(tex.id(), Rect::from_min_size(Pos2::new(x, y), sz), uv, Color32::WHITE);
                    x += sz.x;
                } else {
                    let fallback = format!("#S{code}");
                    let g = painter.layout_no_wrap(fallback.clone(), font.clone(), fill);
                    let sz = g.size();
                    let y = start.y + (line_h - sz.y) * 0.5;
                    draw_halo_text(painter, Pos2::new(x, y), &fallback, font, fill, halo);
                    x += sz.x;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_dream_label_badge(
    painter: &egui::Painter,
    anchor: Pos2,
    title: &str,
    subtitle: &str,
    font: &FontId,
    max_w: f32,
    smileys: &[Option<TextureHandle>],
    scale: f32,
) {
    let mut rows: Vec<(Vec<BadgeSeg>, Vec2, Color32)> = Vec::new();
    if !title.trim().is_empty() {
        let segs = parse_furc_badge_segments(title);
        for line in wrap_badge_segments(painter, &segs, font, max_w, smileys) {
            let sz = measure_badge_line(painter, &line, font, smileys);
            rows.push((line, sz, Color32::WHITE));
        }
    }
    if !subtitle.trim().is_empty() {
        let segs = parse_furc_badge_segments(subtitle);
        let sub_halo = Color32::from_rgb(0xc7, 0xcb, 0xe7);
        for line in wrap_badge_segments(painter, &segs, font, max_w, smileys) {
            let sz = measure_badge_line(painter, &line, font, smileys);
            rows.push((line, sz, sub_halo));
        }
    }
    if rows.is_empty() {
        return;
    }
    let line_gap = (1.0 * scale).clamp(0.5, 2.5);
    let total_h: f32 = rows.iter().map(|(_, sz, _)| sz.y).sum::<f32>()
        + line_gap * rows.len().saturating_sub(1) as f32;
    let mut y = anchor.y - total_h * 0.5;
    for (line, sz, halo) in rows {
        let x = anchor.x - sz.x * 0.5;
        draw_badge_line(
            painter,
            Pos2::new(x, y),
            sz.y,
            &line,
            font,
            Color32::BLACK,
            halo,
            smileys,
        );
        y += sz.y + line_gap;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_player_name_badge(
    painter: &egui::Painter,
    viewport: Rect,
    label_pos: Pos2,
    raw_name: &str,
    is_self: bool,
    is_highlighted: bool,
    font: &FontId,
    smileys: &[Option<TextureHandle>],
    scale: f32,
) -> Option<Rect> {
    if !viewport.expand(24.0).contains(label_pos) {
        return None;
    }
    let segs = parse_furc_badge_segments(raw_name);
    if segs.is_empty() {
        return None;
    }
    let content_size = measure_badge_line(painter, &segs, font, smileys);
    if content_size.x <= 0.0 {
        return None;
    }
    let pad_x = (5.5 * scale).clamp(3.0, 9.0);
    let pad_y = (2.0 * scale).clamp(1.5, 4.0);
    let pill_size = content_size + Vec2::new(pad_x * 2.0, pad_y * 2.0);
    let min_x = viewport.min.x + 4.0;
    let max_x = (viewport.max.x - pill_size.x - 4.0).max(min_x);
    let min_y = viewport.min.y + 4.0;
    let max_y = (viewport.max.y - pill_size.y - 4.0).max(min_y);
    let pill_min = Pos2::new(
        (label_pos.x - pill_size.x * 0.5).clamp(min_x, max_x),
        (label_pos.y - pill_size.y * 0.5).clamp(min_y, max_y),
    );
    let pill_rect = Rect::from_min_size(pill_min, pill_size);
    let (fill_color, border_color) = if is_self {
        (
            Color32::from_rgb(0xc6, 0xc7, 0x7c),
            if is_highlighted {
                Color32::from_rgb(0xff, 0xf2, 0x99)
            } else {
                Color32::from_rgb(0x8d, 0x7c, 0x26)
            },
        )
    } else if is_highlighted {
        (
            Color32::from_rgba_unmultiplied(145, 198, 236, 215),
            Color32::from_rgb(0x66, 0xc4, 0xe8),
        )
    } else {
        (
            Color32::from_rgba_unmultiplied(119, 170, 209, 150),
            Color32::from_rgb(0x29, 0x70, 0x8a),
        )
    };
    painter.rect_filled(pill_rect, 3.0, fill_color);
    painter.rect_stroke(pill_rect, 3.0, Stroke::new(1.0_f32, border_color));
    draw_badge_line(
        painter,
        pill_rect.min + Vec2::new(pad_x, pad_y),
        content_size.y,
        &segs,
        font,
        Color32::BLACK,
        Color32::WHITE,
        smileys,
    );
    Some(pill_rect)
}

fn tile_to_screen(x: f32, y: f32, tile_w: f32, tile_h: f32) -> Vec2 {
    let sx = x * tile_w - if (y as i32) & 1 != 0 { tile_w * 0.5 } else { 0.0 };
    let sy = y * (tile_h * 0.5);
    Vec2::new(sx, sy)
}

/// Parses server cookie reports such as `Cookie bank has currently collected: 3` and
/// `All-time Cookie total: 12`, returning `(collected, all_time_total)`.
fn parse_cookie_counts(plain: &str) -> (Option<u32>, Option<u32>) {
    let lower = plain.to_ascii_lowercase();
    if !lower.contains("cookie") {
        return (None, None);
    }
    let after = |needle: &str| -> Option<u32> {
        let idx = lower.find(needle)? + needle.len();
        let digits: String = lower[idx..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    };
    let collected = after("currently collected:").or_else(|| {
        let idx = lower.find("item: cookies (")? + "item: cookies (".len();
        let digits: String = lower[idx..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    });
    let total_or_inactive = after("all-time cookie total:").or_else(|| {
        let idx = lower.find("item: cookies (")?;
        let sub = &lower[idx..];
        let act_idx = sub.find("active,")? + "active,".len();
        let digits: String = sub[act_idx..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    });
    (collected, total_or_inactive)
}

fn extract_whisper_peer(plain: &str) -> Option<String> {
    let s = plain.trim();
    if let Some(idx) = s.find(" whispers,") {
        let before = s[..idx].trim().trim_start_matches('[').trim().replace('|', " ");
        if !before.is_empty() {
            return Some(before);
        }
    }
    if let Some((target, _)) = extract_outgoing_whisper_echo(s) {
        if !target.trim().is_empty() {
            return Some(target.trim().to_owned());
        }
    }
    None
}

/// Resolves a character's custom portrait `pp<id>`, preferring known server `]&` IDs
/// and falling back to `clogin.php`'s `lastport` only when it exceeds the 1..=16 slot range.
fn resolve_custom_portrait_id(
    player_portrait_ids: &HashMap<String, u32>,
    name_key: &str,
    last_port: Option<u32>,
) -> Option<u32> {
    if name_key.is_empty() {
        return None;
    }
    player_portrait_ids
        .get(name_key)
        .copied()
        .or_else(|| {
            player_portrait_ids
                .iter()
                .find(|(known_name, _)| shortname_key(known_name) == name_key)
                .map(|(_, &pid)| pid)
        })
        .or_else(|| last_port.filter(|&p| p > 16))
}

fn parse_portrait_directive(raw: &str) -> (Option<u32>, Option<u32>, Option<String>) {
    let mut character_id = None;
    let mut portrait_id = None;
    let mut name_parts = Vec::new();

    for token in raw.split_whitespace() {
        if let Some(id) = token
            .get(..2)
            .filter(|prefix| prefix.eq_ignore_ascii_case("pp"))
            .and_then(|_| token.get(2..))
            .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|id| id.parse::<u32>().ok())
            .filter(|id| *id > 0)
        {
            portrait_id = Some(id);
        } else if let Ok(id) = token.parse::<u32>() {
            if character_id.is_none() && id > 0 {
                character_id = Some(id);
            }
        } else {
            name_parts.push(token);
        }
    }

    let name = (!name_parts.is_empty()).then(|| wire_player_name(&name_parts.join(" ")));
    (character_id, portrait_id, name)
}

fn should_suppress_silent_tab_glook_text(
    pending: &mut Vec<SilentTabGlook>,
    plain: &str,
    now: Instant,
) -> bool {
    pending.retain(|e| now.saturating_duration_since(e.sent_at) <= Duration::from_secs(5));
    let trimmed = plain.trim();
    let seen_name = trimmed
        .strip_prefix("(You see ")
        .and_then(|s| s.split(".)").next())
        .or_else(|| trimmed.strip_prefix("You see ").and_then(|s| s.split('.').next()));
    if let Some(name) = seen_name {
        let seen_key = shortname_key(name);
        if !seen_key.is_empty() {
            if let Some(entry) = pending.iter_mut().find(|e| e.target_key == seen_key) {
                entry.saw_you_see = true;
                return true;
            }
        }
    } else if trimmed.starts_with("> ") {
        if let Some(idx) = pending
            .iter()
            .rposition(|e| e.saw_you_see && now.saturating_duration_since(e.sent_at) <= Duration::from_secs(3))
        {
            pending[idx].saw_you_see = false;
            if pending[idx].card_received {
                pending.remove(idx);
            }
            return true;
        }
    }
    false
}

fn cache_looked_player_card_into(
    card: &LookedPlayerCard,
    known_player_cards: &mut HashMap<String, LookedPlayerCard>,
    whispers: &mut [WhisperSession],
) {
    let mut key = shortname_key(&card.short_name);
    if key.is_empty() {
        key = shortname_key(&card.info.name);
    }
    if key.is_empty() {
        return;
    }
    if known_player_cards.len() >= MAX_CUSTOM_PORTRAITS && !known_player_cards.contains_key(&key) {
        known_player_cards.clear();
    }
    known_player_cards.insert(key.clone(), card.clone());
    for w in whispers {
        if shortname_key(&w.peer) == key {
            w.looked_card = Some(card.clone());
            w.portrait_fetched = true;
        }
    }
}

fn resolve_whisper_peer_card_from_sources(
    peer: &str,
    whispers: &[WhisperSession],
    known_player_cards: &HashMap<String, LookedPlayerCard>,
    player_portrait_ids: &HashMap<String, u32>,
    avatars: &std::collections::BTreeMap<u32, Avatar>,
) -> Option<LookedPlayerCard> {
    let peer_key = shortname_key(peer);
    if peer_key.is_empty() {
        return None;
    }
    let known_pid = player_portrait_ids.get(&peer_key).copied();
    if let Some(mut card) = whispers
        .iter()
        .find(|w| w.peer.eq_ignore_ascii_case(peer) || shortname_key(&w.peer) == peer_key)
        .and_then(|w| w.looked_card.clone())
    {
        card.portrait_id = known_pid.or(card.portrait_id);
        return Some(card);
    }
    if let Some(mut card) = known_player_cards.get(&peer_key).cloned() {
        card.portrait_id = known_pid.or(card.portrait_id);
        return Some(card);
    }
    if let Some(av) = avatars
        .values()
        .find(|av| shortname_key(&av.name) == peer_key)
    {
        return Some(LookedPlayerCard {
            info: LookedPlayerInfo {
                name: av.name.clone(),
                appearance: av.appearance.clone(),
            },
            short_name: wire_player_name(&av.name),
            portrait_id: known_pid,
            portrait_tex: None,
            badge_tex: None,
            resolved_for: None,
        });
    }
    known_pid.map(|pid| LookedPlayerCard {
        info: LookedPlayerInfo {
            name: peer.replace('|', " "),
            appearance: AvatarAppearance::default(),
        },
        short_name: wire_player_name(peer),
        portrait_id: Some(pid),
        portrait_tex: None,
        badge_tex: None,
        resolved_for: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn sync_tab_portrait_state(
    active_chat_tab: Option<&str>,
    is_connected: bool,
    looked_player: &mut Option<LookedPlayerCard>,
    main_looked_player: &mut Option<LookedPlayerCard>,
    whispers: &mut [WhisperSession],
    known_player_cards: &HashMap<String, LookedPlayerCard>,
    player_portrait_ids: &HashMap<String, u32>,
    avatars: &std::collections::BTreeMap<u32, Avatar>,
) -> Option<String> {
    let Some(peer) = active_chat_tab else {
        *looked_player = main_looked_player.clone();
        if let Some(card) = looked_player.as_mut() {
            if let Some(&pid) = player_portrait_ids.get(&shortname_key(&card.short_name)) {
                card.portrait_id = Some(pid);
            }
        }
        return None;
    };

    let peer_key = shortname_key(peer);
    let resolved = resolve_whisper_peer_card_from_sources(
        peer,
        whispers,
        known_player_cards,
        player_portrait_ids,
        avatars,
    );
    *looked_player = resolved.clone();
    let has_server_card = known_player_cards.contains_key(&peer_key);
    let mut should_fetch = false;
    if let Some(w) = whispers
        .iter_mut()
        .find(|w| w.peer.eq_ignore_ascii_case(peer) || shortname_key(&w.peer) == peer_key)
    {
        if w.looked_card.is_none() && resolved.is_some() {
            w.looked_card = resolved;
        }
        if has_server_card {
            w.portrait_fetched = true;
        } else if is_connected && !w.portrait_fetched {
            w.portrait_fetched = true;
            should_fetch = true;
        }
    }
    should_fetch.then(|| peer.to_owned())
}

#[allow(clippy::too_many_arguments)]
fn apply_looked_card_to_tabs(
    card: LookedPlayerCard,
    active_chat_tab: Option<&str>,
    looked_player: &mut Option<LookedPlayerCard>,
    main_looked_player: &mut Option<LookedPlayerCard>,
    whispers: &mut [WhisperSession],
    known_player_cards: &mut HashMap<String, LookedPlayerCard>,
    pending_silent_tab_glooks: &mut Vec<SilentTabGlook>,
    now: Instant,
) {
    let short_key = shortname_key(&card.short_name);
    cache_looked_player_card_into(&card, known_player_cards, whispers);
    pending_silent_tab_glooks.retain(|e| now.saturating_duration_since(e.sent_at) <= Duration::from_secs(5));
    if let Some(entry) = pending_silent_tab_glooks
        .iter_mut()
        .find(|e| e.target_key == short_key && !e.card_received)
    {
        entry.card_received = true;
        if active_chat_tab.is_some_and(|peer| shortname_key(peer) == short_key) {
            *looked_player = Some(card);
        }
    } else {
        *looked_player = Some(card.clone());
        *main_looked_player = Some(card);
    }
}

fn match_pending_whisper_echo(
    plain: &str,
    self_name: &str,
    pending: &mut Vec<(String, String, Instant)>,
) -> Option<String> {
    let now = Instant::now();
    let max_age = std::time::Duration::from_secs(10);
    pending.retain(|(_, _, sent_at)| now.duration_since(*sent_at) < max_age);
    let (speaker, body) = plain.trim().split_once(':')?;
    if !speaker.trim().eq_ignore_ascii_case(&self_name.replace('|', " ")) {
        return None;
    }
    let body = normalize_whisper_body(body);
    let idx = pending
        .iter()
        .position(|(_, message, _)| message.eq_ignore_ascii_case(&body))?;
    Some(pending.remove(idx).0)
}

fn normalize_whisper_body(message: &str) -> String {
    parse_chat_line(message).to_plain_text().trim().to_owned()
}

fn extract_outgoing_whisper_echo(plain: &str) -> Option<(String, String)> {
    let s = plain
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    let start = s.find("You whisper ")?;
    let rest = &s[start..];
    let to_idx = rest.rfind(" to ")?;
    let quoted = rest[..to_idx]
        .trim()
        .strip_prefix("You whisper ")?
        .trim();
    let body = quoted
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .unwrap_or(quoted);
    let target = rest[to_idx + 4..]
        .trim()
        .trim_end_matches('.')
        .trim()
        .replace('|', " ");
    (!target.is_empty()).then(|| (target, body.to_owned()))
}

/// Formats a whisper line for a dedicated peer whisper tab/window in regular chat style
/// (stripping the `[ You whisper "..." to Peer . ]` / `[ Peer whispers, "..." to you. ]` wrapper).
fn is_whisper_colon_emote(text: &str) -> bool {
    let Some(rest) = text.trim().strip_prefix(':') else {
        return false;
    };
    let rest = rest.trim_start();
    if rest.is_empty() {
        return false;
    }
    if rest.starts_with("\x27s ") || rest.starts_with("\x27S ") {
        return true;
    }
    let first_word = rest.split_whitespace().next().unwrap_or("");
    let first_char = first_word.chars().next().unwrap_or(' ');
    if !first_char.is_ascii_alphabetic() {
        return false;
    }
    let stripped = first_word.trim_end_matches(['!', '?', '.', ',']);
    let lower = stripped.to_ascii_lowercase();
    if !lower.is_empty() {
        let first_lower = lower.chars().next().unwrap();
        let all_same = lower.chars().all(|c| c == first_lower);
        if all_same && matches!(first_lower, 'd' | 'p' | 'o' | 's' | 'x' | 'b' | 'c' | 'v' | 'z') {
            return false;
        }
        if matches!(lower.as_str(), "xd" | "xp" | "xo" | "dd" | "pp") {
            return false;
        }
    }
    stripped.len() >= 2
}

fn format_whisper_tab_line(
    self_name: &str,
    peer: &str,
    plain: &str,
    fallback: &ChatLine,
) -> ChatLine {
    let s = plain.trim().trim_start_matches('[').trim_end_matches(']').trim();
    let (speaker, body) = if let Some(rest) = s.strip_prefix("You whisper ") {
        if let Some(to_idx) = rest.rfind(" to ") {
            let quoted = rest[..to_idx].trim();
            let inner = quoted
                .strip_prefix('"')
                .and_then(|q| q.strip_suffix('"'))
                .unwrap_or(quoted);
            (self_name, inner)
        } else {
            return fallback.clone();
        }
    } else if let Some(w_idx) = s.find(" whispers,") {
        let after = s[w_idx + " whispers,".len()..].trim();
        if let Some(to_idx) = after.rfind(" to you") {
            let quoted = after[..to_idx].trim();
            let inner = quoted
                .strip_prefix('"')
                .and_then(|q| q.strip_suffix('"'))
                .unwrap_or(quoted);
            (peer, inner)
        } else if let Some(quoted) = after.strip_prefix('"') {
            // Some emote whispers arrive without Furcadia's usual `to you`
            // suffix. Recover the quoted body instead of displaying the raw
            // server wrapper as a normal chat line.
            let inner = quoted
                .rfind('"')
                .map(|end| &quoted[..end])
                .unwrap_or(quoted)
                .trim();
            (peer, inner)
        } else {
            return fallback.clone();
        }
    } else {
        return fallback.clone();
    };

    let trimmed = body.trim();
    let speaker_disp = speaker.replace('|', " ");
    let speaker_space = speaker_disp.to_ascii_lowercase();
    let speaker_pipe = speaker.replace(' ', "|").to_ascii_lowercase();
    let lower = trimmed.to_ascii_lowercase();
    let is_name_prefixed_emote = lower.starts_with(&format!("{speaker_space} "))
        || lower.starts_with(&format!("{speaker_pipe} "))
        || lower.starts_with(&format!("{speaker_space}'"))
        || lower.starts_with(&format!("{speaker_pipe}'"));
    let escape_html = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;");
    let short_name_attr = wire_player_name(speaker)
        .replace('&', "&amp;")
        .replace('\'', "&#39;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let speaker_tag = format!(
        "<name shortname='{short_name_attr}'>{}</name>",
        escape_html(&speaker_disp)
    );
    if is_whisper_colon_emote(trimmed) {
        let emote = trimmed.strip_prefix(':').unwrap_or(trimmed);
        let emote_body = emote.trim_start();
        let sep = if emote_body.starts_with('\x27') { "" } else { " " };
        let line_html = format!(
            "<font color='emote'>{speaker_tag}{sep}{}</font>",
            escape_html(emote_body)
        );
        parse_chat_line(&line_html)
    } else if is_name_prefixed_emote {
        let prefix_len = if lower.starts_with(&speaker_pipe) {
            speaker_pipe.len()
        } else {
            speaker_space.len()
        };
        parse_chat_line(&format!(
            "<font color='emote'>{speaker_tag}{}</font>",
            escape_html(&trimmed[prefix_len..])
        ))
    } else {
        parse_chat_line(&format!(
            "{speaker_tag}: {}",
            escape_html(trimmed)
        ))
    }
}

fn render_chat_image_span(
    ui: &mut egui::Ui,
    src: &str,
    alt: Option<&str>,
    smileys: &[Option<TextureHandle>],
    desctags: &[Option<TextureHandle>],
    chat_imgs: &HashMap<String, Option<TextureHandle>>,
) {
    let trimmed_alt = alt.map(str::trim).unwrap_or("");
    let lower_alt = trimmed_alt.to_ascii_lowercase();
    let lower_src = src.trim().to_ascii_lowercase();
    let font_id = egui::TextStyle::Body.resolve(ui.style());

    if lower_src.starts_with("http://") || lower_src.starts_with("https://") {
        if let Some(Some(tex)) = chat_imgs.get(src.trim()) {
            let sz = tex.size_vec2();
            let target_h = (font_id.size * 1.45).clamp(14.0, 26.0);
            let draw_sz = if sz.y > 0.0 {
                Vec2::new(sz.x * (target_h / sz.y), target_h)
            } else {
                Vec2::new(target_h, target_h)
            };
            let resp = ui.image(egui::load::SizedTexture::new(tex.id(), draw_sz));
            if !trimmed_alt.is_empty() {
                resp.on_hover_text(trimmed_alt);
            }
            return;
        }
    }

    let parsed_desctag_idx = lower_src
        .strip_prefix("fsh://desctags.fox:")
        .or_else(|| lower_src.strip_prefix("fsh://desctags:"))
        .or_else(|| lower_src.strip_prefix("fsh://desctag:"))
        .and_then(|s| s.trim().parse::<usize>().ok());
    if let Some(idx) = parsed_desctag_idx {
        if let Some(tex) = desctags.get(idx).and_then(|o| o.as_ref()) {
            let sz = smiley_draw_size(tex, &font_id);
            let resp = ui.image(egui::load::SizedTexture::new(tex.id(), sz));
            if !trimmed_alt.is_empty() {
                resp.on_hover_text(trimmed_alt);
            }
            return;
        }
    }

    let parsed_smiley_idx = lower_src
        .strip_prefix("fsh://smileys.fsh:")
        .and_then(|s| s.trim().parse::<usize>().ok())
        .or_else(|| {
            lower_src
                .strip_prefix("fsh://smileys2.fsh:")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .map(|i| i + 26)
        });
    if let Some(idx) = parsed_smiley_idx {
        if let Some(tex) = smileys.get(idx).and_then(|o| o.as_ref()) {
            let sz = smiley_draw_size(tex, &font_id);
            let pad = Vec2::new(4.0, 2.0);
            let (rect, resp) = ui.allocate_exact_size(sz + pad * 2.0, Sense::hover());
            ui.painter().rect_filled(rect, 4.0, Color32::from_rgb(225, 222, 232));
            ui.painter().rect_stroke(
                rect,
                4.0,
                Stroke::new(1.0_f32, Color32::from_rgb(155, 148, 175)),
            );
            let img_rect = Rect::from_center_size(rect.center(), sz);
            ui.painter().image(
                tex.id(),
                img_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
            if !trimmed_alt.is_empty() {
                resp.on_hover_text(trimmed_alt);
            }
            return;
        }
    }

    let (glyph, color): (&str, Color32) = if lower_alt == "@emit" || lower_alt == "emit" {
        ("[*]", Color32::from_rgb(245, 205, 110))
    } else if lower_alt == "@events" || lower_alt == "events" || lower_alt.contains("star") {
        ("*", GOLD)
    } else if lower_alt == "@paw" || lower_alt.contains("paw") {
        ("[paw]", Color32::from_rgb(175, 195, 235))
    } else if !trimmed_alt.is_empty() {
        ui.label(
            RichText::new(format!("[{trimmed_alt}]")).color(Color32::from_rgb(170, 175, 195)),
        );
        return;
    } else if let Some(idx_str) = lower_src.strip_prefix("fsh://system.fsh:") {
        match idx_str.trim() {
            "86" => ("›", Color32::from_rgb(232, 160, 228)),
            "85" => ("[paw]", Color32::from_rgb(175, 195, 235)),
            "84" => ("*", Color32::from_rgb(245, 205, 110)),
            "81" | "82" | "83" => ("*", GOLD),
            _ => ("•", Color32::from_rgb(170, 175, 195)),
        }
    } else if lower_src.contains("smiley") || lower_src.contains("#s") {
        (":)", Color32::from_rgb(245, 215, 120))
    } else {
        ("•", Color32::from_rgb(170, 175, 195))
    };

    ui.label(RichText::new(glyph).color(color));
}

#[derive(Debug, Clone)]
enum ChatClickAction {
    OpenPlayerMenu {
        display_name: String,
        short_name: String,
        pos: Pos2,
    },
    EnterDreamUrl(String),
    SendCommand(String),
}

fn desctag_index_for_code(ch: char) -> Option<usize> {
    let b = ch as u32;
    if (0x20..=0xff).contains(&b) {
        Some((b - 0x20) as usize)
    } else {
        None
    }
}

enum InlineChatPart<'a> {
    Text(&'a str),
    Smiley { idx: usize, code: char },
    Desctag { idx: usize, code: char },
}

fn split_inline_chat_codes(s: &str) -> Vec<InlineChatPart<'_>> {
    let mut parts = Vec::new();
    let bytes = s.as_bytes();
    let mut last = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        // Furcadia chat logs can contain a literal uppercase `:D` face. Keep
        // this case-sensitive and require a token boundary so `:Decides`
        // remains ordinary emote text instead of becoming a face plus text.
        if bytes[i] == b':'
            && bytes[i + 1] == b'D'
            && (i + 2 == bytes.len() || !bytes[i + 2].is_ascii_alphabetic())
        {
            if i > last {
                parts.push(InlineChatPart::Text(&s[last..i]));
            }
            if let Some(idx) = smiley_index_for_code('D') {
                parts.push(InlineChatPart::Smiley { idx, code: 'D' });
            }
            i += 2;
            last = i;
            continue;
        }
        if i + 2 < bytes.len()
            && bytes[i] == b'#'
            && (bytes[i + 1] == b'S' || bytes[i + 1] == b'C')
        {
            if let Some(ch) = s[i + 2..].chars().next() {
                let kind = bytes[i + 1];
                let mapped = if kind == b'S' {
                    smiley_index_for_code(ch).map(|idx| InlineChatPart::Smiley { idx, code: ch })
                } else {
                    desctag_index_for_code(ch).map(|idx| InlineChatPart::Desctag { idx, code: ch })
                };
                if let Some(part) = mapped {
                    if i > last {
                        parts.push(InlineChatPart::Text(&s[last..i]));
                    }
                    parts.push(part);
                    i += 2 + ch.len_utf8();
                    last = i;
                    continue;
                }
            }
        }
        if let Some(ch) = s[i..].chars().next() {
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    if last < s.len() {
        parts.push(InlineChatPart::Text(&s[last..]));
    }
    parts
}

fn name_badge_chat_color(is_self: bool) -> Color32 {
    if is_self {
        // Match the olive fill used by the local player's map name badge.
        Color32::from_rgb(0xc6, 0xc7, 0x7c)
    } else {
        // Match the blue fill used by other players' map name badges.
        Color32::from_rgb(119, 170, 209)
    }
}

/// Renders a single parsed chat line with inline badges, smileys, links, and click actions.
#[allow(clippy::too_many_arguments)] // Passes read-only texture caches and theme settings directly.
fn render_chat_line(
    ui: &mut egui::Ui,
    line: &ChatLine,
    action_out: &mut Option<ChatClickAction>,
    smileys: &[Option<TextureHandle>],
    desctags: &[Option<TextureHandle>],
    chat_imgs: &HashMap<String, Option<TextureHandle>>,
    player_badge_colors: &HashMap<String, Color32>,
    player_badges: &HashMap<String, TextureHandle>,
    self_name_key: &str,
    colors: &UiColors,
) {
    let has_name_link = line.spans.iter().any(|s| {
        matches!(
            s,
            ChatSpan::Text { style, .. }
                if style
                    .link
                    .as_deref()
                    .is_some_and(|u| u.starts_with("name://"))
        )
    });
    let fallback_peer = if has_name_link {
        None
    } else {
        let plain = line.to_plain_text();
        extract_whisper_peer(&plain).or_else(|| {
            plain
                .trim()
                .strip_prefix("(You see ")
                .and_then(|s| s.strip_suffix(".)"))
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
    };

    let speaker_badge = chat_line_speaker_key(line).and_then(|key| player_badges.get(&key));
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        if let Some(badge) = speaker_badge {
            let tex_size = badge.size_vec2();
            let height = (ui.text_style_height(&egui::TextStyle::Body) * 0.9).clamp(10.0, 16.0);
            let width = if tex_size.y > 0.0 {
                (tex_size.x * height / tex_size.y).clamp(12.0, 22.0)
            } else {
                height
            };
            ui.image((badge.id(), Vec2::new(width, height)));
        }
        for span in &line.spans {
            match span {
                ChatSpan::Image { src, alt } => {
                    render_chat_image_span(ui, src, alt.as_deref(), smileys, desctags, chat_imgs);
                }
                ChatSpan::Text { text, style } => {
                    let is_emote = style
                        .color
                        .as_deref()
                        .is_some_and(|c| c.eq_ignore_ascii_case("emote"));
                    let is_fallback_peer = style.link.is_none()
                        && fallback_peer
                            .as_deref()
                            .is_some_and(|peer| !peer.is_empty() && text.contains(peer));
                    let split_quotes = is_emote && style.link.is_none() && text.contains('"');
                    let highlight_brackets = !is_fallback_peer
                        && should_highlight_brackets(style)
                        && text.contains('[')
                        && text.contains(']');
                    if split_quotes || highlight_brackets {
                        let font_id = egui::TextStyle::Body.resolve(ui.style());
                        let segments =
                            split_chat_segments(text, split_quotes, highlight_brackets);
                        for (seg, kind) in segments {
                            if seg.is_empty() {
                                continue;
                            }
                            for part in split_inline_chat_codes(seg) {
                                match part {
                                    InlineChatPart::Text(sub) if !sub.is_empty() => {
                                        let mut rt = RichText::new(sub);
                                        if style.bold {
                                            rt = apply_chat_bold(ui, rt);
                                        }
                                        if style.underline {
                                            rt = rt.underline();
                                        }
                                        match kind {
                                            ChatSegmentKind::Bracketed => {
                                                if style.italic && !is_emote {
                                                    rt = rt.italics();
                                                }
                                                rt = rt.color(colors.bracket_text);
                                            }
                                            ChatSegmentKind::EmoteQuote => {
                                                rt = rt.color(colors.chat_text);
                                            }
                                            ChatSegmentKind::Normal => {
                                                if style.italic || is_emote {
                                                    rt = rt.italics();
                                                }
                                                let col = style
                                                    .color
                                                    .as_deref()
                                                    .map(|c| colors.map_chat_color(c))
                                                    .unwrap_or(colors.chat_text);
                                                rt = rt.color(col);
                                            }
                                        }
                                        ui.label(rt);
                                    }
                                    InlineChatPart::Smiley { idx, code } => {
                                        if let Some(tex) = smileys.get(idx).and_then(|o| o.as_ref()) {
                                            render_inline_smiley_badge(ui, tex, &font_id);
                                        } else {
                                            ui.label(RichText::new(format!("#S{code}")));
                                        }
                                    }
                                    InlineChatPart::Desctag { idx, code } => {
                                        if let Some(tex) = desctags.get(idx).and_then(|o| o.as_ref()) {
                                            let sz = smiley_draw_size(tex, &font_id);
                                            ui.image(egui::load::SizedTexture::new(tex.id(), sz));
                                        } else {
                                            ui.label(RichText::new(format!("#C{code}")));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        continue;
                    }
                    let mut rt = RichText::new(text);
                    if style.bold {
                        rt = apply_chat_bold(ui, rt);
                    }
                    if style.italic || is_emote {
                        rt = rt.italics();
                    }
                    if style.underline || style.link.is_some() {
                        rt = rt.underline();
                    }
                    if let Some(color_name) = &style.color {
                        rt = rt.color(colors.map_chat_color(color_name));
                    } else {
                        rt = rt.color(colors.chat_text);
                    }
                    if let Some(url) = &style.link {
                        if text.contains("#S") || text.contains("#C") {
                            render_linked_inline_codes(
                                ui,
                                text,
                                style,
                                url,
                                action_out,
                                smileys,
                                desctags,
                                player_badge_colors,
                                self_name_key,
                                colors,
                            );
                            continue;
                        }
                        let lower = url.to_ascii_lowercase();
                        if let Some(short) = url.strip_prefix("name://") {
                            let key = shortname_key(short);
                            let disp_key = shortname_key(text);
                            rt = rt.color(player_badge_colors
                                .get(&key)
                                .or_else(|| player_badge_colors.get(&disp_key))
                                .copied()
                                .unwrap_or_else(|| name_badge_chat_color(key == self_name_key)));
                            let resp = ui
                                .add(egui::Label::new(rt).sense(egui::Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(format!("{text}\nPlayer: {short}"));
                            if resp.secondary_clicked() {
                                let pos = resp
                                    .interact_pointer_pos()
                                    .unwrap_or_else(|| resp.rect.left_bottom());
                                *action_out = Some(ChatClickAction::OpenPlayerMenu {
                                    display_name: text.trim().to_owned(),
                                    short_name: short.to_owned(),
                                    pos,
                                });
                            }
                        } else if lower.starts_with("furc://") {
                            let resp = ui
                                .add(egui::Label::new(rt).sense(egui::Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(format!("{text}\n{url}"));
                            if resp.clicked() {
                                *action_out = Some(ChatClickAction::EnterDreamUrl(url.clone()));
                            }
                        } else if lower.starts_with("command://") {
                            let cmd = url[10..].trim_start_matches('/').to_owned();
                            let resp = ui
                                .add(egui::Label::new(rt).sense(egui::Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(format!("{text}\nCommand: {cmd}"));
                            if resp.clicked() {
                                *action_out = Some(ChatClickAction::SendCommand(cmd));
                            }
                        } else {
                            ui.hyperlink_to(rt, url)
                                .on_hover_text(format!("{text}\n{url}"));
                        }
                    } else if fallback_peer
                        .as_deref()
                        .is_some_and(|peer| !peer.is_empty() && text.contains(peer))
                    {
                        let peer = fallback_peer.as_deref().unwrap_or_default();
                        let key = shortname_key(peer);
                        rt = rt.color(player_badge_colors
                            .get(&key)
                            .copied()
                            .unwrap_or_else(|| name_badge_chat_color(key == self_name_key)));
                        let resp = ui
                            .add(egui::Label::new(rt.underline()).sense(egui::Sense::click()))
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(format!("{peer}\nPlayer profile and actions"));
                        if resp.secondary_clicked() {
                            let pos = resp
                                .interact_pointer_pos()
                                .unwrap_or_else(|| resp.rect.left_bottom());
                            *action_out = Some(ChatClickAction::OpenPlayerMenu {
                                display_name: peer.to_owned(),
                                short_name: wire_player_name(peer),
                                pos,
                            });
                        }
                    } else if text.contains("#S") || text.contains("#C") {
                        let font_id = egui::TextStyle::Body.resolve(ui.style());
                        for part in split_inline_chat_codes(text) {
                            match part {
                                InlineChatPart::Text(seg) if !seg.is_empty() => {
                                    let mut seg_rt = RichText::new(seg);
                                    if style.bold {
                                        seg_rt = apply_chat_bold(ui, seg_rt);
                                    }
                                    if style.italic || is_emote {
                                        seg_rt = seg_rt.italics();
                                    }
                                    if style.underline {
                                        seg_rt = seg_rt.underline();
                                    }
                                    if let Some(color_name) = &style.color {
                                        seg_rt = seg_rt.color(colors.map_chat_color(color_name));
                                    } else {
                                        seg_rt = seg_rt.color(colors.chat_text);
                                    }
                                    ui.label(seg_rt);
                                }
                                InlineChatPart::Smiley { idx, code } => {
                                    if let Some(tex) = smileys.get(idx).and_then(|o| o.as_ref()) {
                                        render_inline_smiley_badge(ui, tex, &font_id);
                                    } else {
                                        ui.label(RichText::new(format!("#S{code}")));
                                    }
                                }
                                InlineChatPart::Desctag { idx, code } => {
                                    if let Some(tex) = desctags.get(idx).and_then(|o| o.as_ref()) {
                                        let sz = smiley_draw_size(tex, &font_id);
                                        ui.image(egui::load::SizedTexture::new(tex.id(), sz));
                                    } else {
                                        ui.label(RichText::new(format!("#C{code}")));
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else {
                        ui.label(rt);
                    }
                }
            }
        }
    });
}

fn apply_chat_bold(ui: &egui::Ui, rt: RichText) -> RichText {
    let body = egui::TextStyle::Body.resolve(ui.style());
    let bold_family = egui::FontFamily::Name(FONT_FAMILY_UI_BOLD.into());
    let has_bold = body.family == egui::FontFamily::Proportional
        && ui.fonts(|f| f.lock().fonts.definitions().families.contains_key(&bold_family));
    if has_bold {
        rt.family(bold_family).strong()
    } else {
        rt.strong()
    }
}

#[allow(clippy::too_many_arguments)]
fn render_linked_inline_codes(
    ui: &mut egui::Ui,
    text: &str,
    style: &furcadia_client::ChatStyle,
    url: &str,
    action_out: &mut Option<ChatClickAction>,
    smileys: &[Option<TextureHandle>],
    desctags: &[Option<TextureHandle>],
    player_badge_colors: &HashMap<String, Color32>,
    self_name_key: &str,
    colors: &UiColors,
) {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let lower_url = url.to_ascii_lowercase();
    for part in split_inline_chat_codes(text) {
        match part {
            InlineChatPart::Text(segment) if !segment.is_empty() => {
                let mut rich = RichText::new(segment);
                if style.bold {
                    rich = apply_chat_bold(ui, rich);
                }
                if style.italic || style.color.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("emote")) {
                    rich = rich.italics();
                }
                if style.underline || style.link.is_some() {
                    rich = rich.underline();
                }
                if let Some(color_name) = &style.color {
                    rich = rich.color(colors.map_chat_color(color_name));
                } else {
                    rich = rich.color(colors.chat_text);
                }

                if let Some(short) = url.strip_prefix("name://") {
                    let key = shortname_key(short);
                    let display_key = shortname_key(segment);
                    rich = rich.color(
                        player_badge_colors
                            .get(&key)
                            .or_else(|| player_badge_colors.get(&display_key))
                            .copied()
                            .unwrap_or_else(|| name_badge_chat_color(key == self_name_key)),
                    );
                    let response = ui
                        .add(egui::Label::new(rich).sense(egui::Sense::click()))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(format!("{segment}\nPlayer: {short}"));
                    if response.secondary_clicked() {
                        let pos = response
                            .interact_pointer_pos()
                            .unwrap_or_else(|| response.rect.left_bottom());
                        *action_out = Some(ChatClickAction::OpenPlayerMenu {
                            display_name: segment.trim().to_owned(),
                            short_name: short.to_owned(),
                            pos,
                        });
                    }
                } else if lower_url.starts_with("furc://") {
                    let response = ui
                        .add(egui::Label::new(rich).sense(egui::Sense::click()))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(format!("{segment}\n{url}"));
                    if response.clicked() {
                        *action_out = Some(ChatClickAction::EnterDreamUrl(url.to_owned()));
                    }
                } else if lower_url.starts_with("command://") {
                    let command = url[10..].trim_start_matches('/').to_owned();
                    let response = ui
                        .add(egui::Label::new(rich).sense(egui::Sense::click()))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(format!("{segment}\nCommand: {command}"));
                    if response.clicked() {
                        *action_out = Some(ChatClickAction::SendCommand(command));
                    }
                } else {
                    ui.hyperlink_to(rich, url)
                        .on_hover_text(format!("{segment}\n{url}"));
                }
            }
            InlineChatPart::Smiley { idx, code } => {
                if let Some(tex) = smileys.get(idx).and_then(|tex| tex.as_ref()) {
                    render_inline_smiley_badge(ui, tex, &font_id);
                } else {
                    ui.label(RichText::new(format!("#S{code}")));
                }
            }
            InlineChatPart::Desctag { idx, code } => {
                if let Some(tex) = desctags.get(idx).and_then(|tex| tex.as_ref()) {
                    let size = smiley_draw_size(tex, &font_id);
                    ui.image(egui::load::SizedTexture::new(tex.id(), size));
                } else {
                    ui.label(RichText::new(format!("#C{code}")));
                }
            }
            _ => {}
        }
    }
}

fn chat_line_speaker_key(line: &ChatLine) -> Option<String> {
    line.spans.iter().find_map(|span| match span {
        ChatSpan::Text { style, .. } => style
            .link
            .as_deref()
            .and_then(|url| url.strip_prefix("name://"))
            .map(shortname_key),
        _ => None,
    })
}

fn rgb_from_packed(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn packed_from_rgb(color: Color32) -> u32 {
    (u32::from(color.r()) << 16) | (u32::from(color.g()) << 8) | u32::from(color.b())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UiColors {
    chat_text: Color32,
    my_speech: Color32,
    bracket_text: Color32,
    emote_text: Color32,
    whisper_text: Color32,
    emit_text: Color32,
    shout_text: Color32,
    roll_text: Color32,
    success_text: Color32,
    warning_text: Color32,
    error_text: Color32,
    input_text: Color32,
    tab_active_bg: Color32,
    tab_active_text: Color32,
    tab_unread_bg: Color32,
    tab_unread_text: Color32,
    tab_inactive_bg: Color32,
    tab_inactive_text: Color32,
    custom_ui_bg: bool,
    chat_bg: Color32,
    panel_bg: Color32,
    accent: Color32,
}

impl Default for UiColors {
    fn default() -> Self {
        Self::from_config(&ClientConfig::default())
    }
}

impl UiColors {
    fn from_config(cfg: &ClientConfig) -> Self {
        Self {
            chat_text: rgb_from_packed(cfg.chat_text_color),
            my_speech: rgb_from_packed(cfg.my_speech_color),
            bracket_text: rgb_from_packed(cfg.bracket_text_color),
            emote_text: rgb_from_packed(cfg.emote_text_color),
            whisper_text: rgb_from_packed(cfg.whisper_text_color),
            emit_text: rgb_from_packed(cfg.emit_text_color),
            shout_text: rgb_from_packed(cfg.shout_text_color),
            roll_text: rgb_from_packed(cfg.roll_text_color),
            success_text: rgb_from_packed(cfg.success_text_color),
            warning_text: rgb_from_packed(cfg.warning_text_color),
            error_text: rgb_from_packed(cfg.error_text_color),
            input_text: rgb_from_packed(cfg.input_text_color),
            tab_active_bg: rgb_from_packed(cfg.tab_active_bg),
            tab_active_text: rgb_from_packed(cfg.tab_active_text),
            tab_unread_bg: rgb_from_packed(cfg.tab_unread_bg),
            tab_unread_text: rgb_from_packed(cfg.tab_unread_text),
            tab_inactive_bg: rgb_from_packed(cfg.tab_inactive_bg),
            tab_inactive_text: rgb_from_packed(cfg.tab_inactive_text),
            custom_ui_bg: cfg.custom_ui_bg,
            chat_bg: rgb_from_packed(cfg.chat_bg_color),
            panel_bg: rgb_from_packed(cfg.panel_bg_color),
            accent: rgb_from_packed(cfg.accent_color),
        }
    }

    fn write_to_config(&self, cfg: &mut ClientConfig) {
        cfg.chat_text_color = packed_from_rgb(self.chat_text);
        cfg.my_speech_color = packed_from_rgb(self.my_speech);
        cfg.bracket_text_color = packed_from_rgb(self.bracket_text);
        cfg.emote_text_color = packed_from_rgb(self.emote_text);
        cfg.whisper_text_color = packed_from_rgb(self.whisper_text);
        cfg.emit_text_color = packed_from_rgb(self.emit_text);
        cfg.shout_text_color = packed_from_rgb(self.shout_text);
        cfg.roll_text_color = packed_from_rgb(self.roll_text);
        cfg.success_text_color = packed_from_rgb(self.success_text);
        cfg.warning_text_color = packed_from_rgb(self.warning_text);
        cfg.error_text_color = packed_from_rgb(self.error_text);
        cfg.input_text_color = packed_from_rgb(self.input_text);
        cfg.tab_active_bg = packed_from_rgb(self.tab_active_bg);
        cfg.tab_active_text = packed_from_rgb(self.tab_active_text);
        cfg.tab_unread_bg = packed_from_rgb(self.tab_unread_bg);
        cfg.tab_unread_text = packed_from_rgb(self.tab_unread_text);
        cfg.tab_inactive_bg = packed_from_rgb(self.tab_inactive_bg);
        cfg.tab_inactive_text = packed_from_rgb(self.tab_inactive_text);
        cfg.custom_ui_bg = self.custom_ui_bg;
        cfg.chat_bg_color = packed_from_rgb(self.chat_bg);
        cfg.panel_bg_color = packed_from_rgb(self.panel_bg);
        cfg.accent_color = packed_from_rgb(self.accent);
    }

    fn reset_chat_colors(&mut self) {
        let d = Self::default();
        self.chat_text = d.chat_text;
        self.my_speech = d.my_speech;
        self.bracket_text = d.bracket_text;
        self.emote_text = d.emote_text;
        self.whisper_text = d.whisper_text;
        self.emit_text = d.emit_text;
        self.shout_text = d.shout_text;
        self.roll_text = d.roll_text;
        self.success_text = d.success_text;
        self.warning_text = d.warning_text;
        self.error_text = d.error_text;
        self.input_text = d.input_text;
    }

    fn reset_tab_colors(&mut self) {
        let d = Self::default();
        self.tab_active_bg = d.tab_active_bg;
        self.tab_active_text = d.tab_active_text;
        self.tab_unread_bg = d.tab_unread_bg;
        self.tab_unread_text = d.tab_unread_text;
        self.tab_inactive_bg = d.tab_inactive_bg;
        self.tab_inactive_text = d.tab_inactive_text;
    }

    fn reset_window_colors(&mut self) {
        let d = Self::default();
        self.custom_ui_bg = d.custom_ui_bg;
        self.chat_bg = d.chat_bg;
        self.panel_bg = d.panel_bg;
        self.accent = d.accent;
    }

    fn map_chat_color(&self, name: &str) -> Color32 {
        if let Some(hex) = name.trim().strip_prefix('#') {
            if hex.len() == 6 {
                if let (Ok(r), Ok(g), Ok(b)) = (
                    u8::from_str_radix(&hex[0..2], 16),
                    u8::from_str_radix(&hex[2..4], 16),
                    u8::from_str_radix(&hex[4..6], 16),
                ) {
                    return Color32::from_rgb(r, g, b);
                }
            } else if hex.len() == 3 {
                if let (Ok(r), Ok(g), Ok(b)) = (
                    u8::from_str_radix(&hex[0..1], 16),
                    u8::from_str_radix(&hex[1..2], 16),
                    u8::from_str_radix(&hex[2..3], 16),
                ) {
                    return Color32::from_rgb(r * 17, g * 17, b * 17);
                }
            }
        }
        match name.trim().to_ascii_lowercase().as_str() {
            "success" | "green" => self.success_text,
            "warning" | "yellow" | "orange" => self.warning_text,
            "error" | "danger" | "red" => self.error_text,
            "whisper" | "purple" | "magenta" | "trade" | "cmdlink" => self.whisper_text,
            "emote" => self.emote_text,
            "emit" | "bcast" | "broadcast" | "cyan" | "link" | "furclink" | "dragonspeak" => {
                self.emit_text
            }
            "shout" | "blue" | "notify" | "highlight" | "query" => self.shout_text,
            "channel" => Color32::from_rgb(175, 150, 240),
            "desc" | "description" | "history" => Color32::from_rgb(185, 198, 220),
            "roll" => self.roll_text,
            "name" => Color32::from_rgb(145, 200, 245),
            "myspeech" => self.my_speech,
            _ => self.chat_text,
        }
    }
}

fn edit_rgb_color(ui: &mut egui::Ui, color: &mut Color32) -> bool {
    let mut rgb = [color.r(), color.g(), color.b()];
    if ui.color_edit_button_srgb(&mut rgb).changed() {
        *color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
        true
    } else {
        false
    }
}

fn autocorrect_common_words(text: &str) -> String {
    const WORDS: &[(&str, &str)] = &[
        ("teh", "the"), ("adn", "and"), ("taht", "that"), ("hte", "the"),
        ("thier", "their"), ("wierd", "weird"), ("recieve", "receive"),
        ("beleive", "believe"), ("becuase", "because"), ("dont", "don't"),
        ("doesnt", "doesn't"), ("cant", "can't"), ("wont", "won't"),
        ("im", "I'm"), ("youre", "you're"), ("thats", "that's"),
    ];
    text.split_inclusive(char::is_whitespace)
        .map(|part| {
            let start = part.find(|c: char| c.is_ascii_alphabetic() || c == '\'').unwrap_or(part.len());
            let end = part.rfind(|c: char| c.is_ascii_alphabetic() || c == '\'').map_or(start, |i| i + 1);
            if start >= end { return part.to_owned(); }
            let word = &part[start..end];
            let Some((_, replacement)) = WORDS.iter().find(|(wrong, _)| word.eq_ignore_ascii_case(wrong)) else { return part.to_owned(); };
            let replacement = if word.chars().all(|c| !c.is_ascii_lowercase()) {
                replacement.to_ascii_uppercase()
            } else if word.chars().next().is_some_and(char::is_uppercase) {
                let mut chars = replacement.chars();
                chars.next().map(char::to_uppercase).into_iter().flatten().chain(chars).collect()
            } else { (*replacement).to_owned() };
            format!("{}{}{}", &part[..start], replacement, &part[end..])
        })
        .collect()
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatSegmentKind {
    Normal,
    EmoteQuote,
    Bracketed,
}

/// Returns whether inline `[...]` segments in a non-link chat/whisper span should be colored blue.
fn should_highlight_brackets(style: &furcadia_client::ChatStyle) -> bool {
    if style.link.is_some() {
        return false;
    }
    match style.color.as_deref() {
        None => true,
        Some(c) => matches!(
            c.trim().to_ascii_lowercase().as_str(),
            "speech" | "myspeech" | "emote" | "whisper" | "shout"
        ),
    }
}

/// Finds the exclusive byte end index of the `]` matching `s[open_idx] == '['`,
/// requiring at least one byte between `[` and `]` (so empty `[]` is ignored).
fn find_matching_bracket(s: &str, open_idx: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (rel, b) in s[open_idx..].bytes().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return (rel > 1).then_some(open_idx + rel + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits a chat/whisper text span into `Normal`, `EmoteQuote` (`"..."` in emotes),
/// and `Bracketed` (`[...]`) segments while keeping unmatched delimiters normal.
fn split_chat_segments(
    text: &str,
    split_quotes: bool,
    highlight_brackets: bool,
) -> Vec<(&str, ChatSegmentKind)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut seg_start = 0usize;
    let mut i = 0usize;
    let mut in_quote = false;

    while i < bytes.len() {
        if highlight_brackets && bytes[i] == b'[' {
            let limit = if in_quote {
                text[i + 1..]
                    .find('"')
                    .map_or(text.len(), |rel| i + 1 + rel)
            } else {
                text.len()
            };
            if let Some(end) = find_matching_bracket(&text[..limit], i) {
                if i > seg_start {
                    out.push((
                        &text[seg_start..i],
                        if in_quote {
                            ChatSegmentKind::EmoteQuote
                        } else {
                            ChatSegmentKind::Normal
                        },
                    ));
                }
                out.push((&text[i..end], ChatSegmentKind::Bracketed));
                i = end;
                seg_start = end;
                continue;
            }
        }
        if split_quotes && bytes[i] == b'"' {
            if !in_quote {
                if text[i + 1..].find('"').is_some() {
                    if i > seg_start {
                        out.push((&text[seg_start..i], ChatSegmentKind::Normal));
                    }
                    seg_start = i;
                    in_quote = true;
                    i += 1;
                    continue;
                }
            } else {
                out.push((&text[seg_start..=i], ChatSegmentKind::EmoteQuote));
                i += 1;
                seg_start = i;
                in_quote = false;
                continue;
            }
        }
        i += 1;
    }

    if seg_start < text.len() {
        out.push((
            &text[seg_start..],
            if in_quote {
                ChatSegmentKind::EmoteQuote
            } else {
                ChatSegmentKind::Normal
            },
        ));
    }
    out
}

/// Splits an emote string into `(segment, is_quoted_speech)` pairs so `"..."` spoken text
/// renders as normal non-italicized speech while surrounding motion stays italicized.
#[cfg(test)]
fn split_emote_quotes(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        let Some(rel_open) = text[pos..].find('"') else {
            out.push((&text[pos..], false));
            break;
        };
        let open = pos + rel_open;
        let after_open = open + 1;
        let Some(rel_close) = text[after_open..].find('"') else {
            out.push((&text[pos..], false));
            break;
        };
        let close = after_open + rel_close + 1;
        if open > pos {
            out.push((&text[pos..open], false));
        }
        out.push((&text[open..close], true));
        pos = close;
    }
    if out.is_empty() && !text.is_empty() {
        out.push((text, false));
    }
    out
}

/// Rewrites Furcadia's `"Your friend request has been sent to <Name>."` Pounce response
/// to `"<Name> has been added to Pounce"`.
fn rewrite_pounce_add_message(mut line: ChatLine) -> ChatLine {
    let plain = line.to_plain_text();
    let trimmed = plain.trim();
    let prefix = "Your friend request has been sent to";
    // `get` avoids panicking when byte `prefix.len()` lands inside a multi-byte char
    // (e.g. a full-width `Ｅ` in an unrelated message).
    if trimmed
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    {
        let mut link_url: Option<String> = None;
        let mut link_name: Option<String> = None;
        for span in &line.spans {
            if let ChatSpan::Text { text, style } = span {
                if style
                    .link
                    .as_deref()
                    .is_some_and(|u| u.starts_with("name://"))
                    && !text.trim().is_empty()
                {
                    link_url = style.link.clone();
                    link_name = Some(text.trim().to_owned());
                    break;
                }
            }
        }
        let name = link_name.unwrap_or_else(|| {
            trimmed[prefix.len()..]
                .trim()
                .trim_end_matches('.')
                .trim()
                .to_owned()
        });
        if !name.is_empty() {
            let success_style = furcadia_client::chat::ChatStyle {
                color: Some("success".to_owned()),
                ..Default::default()
            };
            let name_style = furcadia_client::chat::ChatStyle {
                color: Some("success".to_owned()),
                link: link_url,
                ..Default::default()
            };
            line.spans = vec![
                ChatSpan::Text {
                    text: name,
                    style: name_style,
                },
                ChatSpan::Text {
                    text: " has been added to Pounce".to_owned(),
                    style: success_style,
                },
            ];
        }
    }
    line
}

fn normalize_main_map_id(lower: &str) -> Option<&'static str> {
    if lower == "furn" || lower == "pmfurn" {
        Some("pmfurn")
    } else if lower == "vinca" || lower == "pmvinca" {
        Some("pmvinca")
    } else if lower.starts_with("acrop") || lower == "pmacrop" || lower == "pmacropolis" {
        Some("pmacropolis")
    } else if lower.starts_with("alleg") || lower == "pmalleg" || lower == "pmallegria" {
        Some("pmallegria")
    } else if lower.starts_with("imagin") || lower == "pmimag" || lower == "pmimaginarium" {
        Some("pmimaginarium")
    } else if lower.starts_with("meov") || lower == "pmmeov" || lower == "pmmeovanni" {
        Some("pmmeovanni")
    } else if lower.starts_with("hawth") || lower == "pmhawth" || lower == "pmhawthorn" {
        Some("pmhawthorn")
    } else if lower.starts_with("oldetown") || lower == "pmoldetown" {
        Some("pmoldetown")
    } else if lower.starts_with("wylde") || lower == "pmwylde" {
        Some("pmwylde")
    } else {
        None
    }
}

fn custom_dictionary_path() -> Option<std::path::PathBuf> {
    default_logs_root()
        .and_then(|d| d.parent().map(|p| p.join("custom_dictionary.txt")))
}

#[allow(dead_code)]
fn expand_user_path(raw: &str) -> std::path::PathBuf {
    let value = raw.trim();
    if value == "~" || value.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let suffix = value.strip_prefix("~/").unwrap_or("");
            return std::path::PathBuf::from(home).join(suffix);
        }
    }
    std::path::PathBuf::from(value)
}

fn load_custom_dictionary() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let Some(path) = custom_dictionary_path() else {
        return set;
    };
    if let Ok(contents) = std::fs::read_to_string(path) {
        for line in contents.lines() {
            let w = line.trim().to_ascii_lowercase();
            if !w.is_empty() {
                set.insert(w);
            }
        }
    }
    set
}

fn find_hunspell_dict() -> Option<String> {
    let candidates = [
        "/usr/share/hunspell/en_US.dic",
        "/usr/share/hunspell/en_GB.dic",
        "/usr/share/myspell/en_US.dic",
        "/usr/share/myspell/dicts/en_US.dic",
    ];
    for p in candidates {
        if std::path::Path::new(p).is_file() {
            return Some(p.trim_end_matches(".dic").to_owned());
        }
    }
    if let Ok(rd) = std::fs::read_dir("/usr/share/wine/gecko") {
        for entry in rd.flatten() {
            let p = entry.path().join("dictionaries/en-US.dic");
            if p.is_file() {
                return Some(p.to_string_lossy().trim_end_matches(".dic").to_owned());
            }
        }
    }
    None
}

fn extract_spellcheck_words(text: &str) -> Vec<(String, usize, usize)> {
    let mut words = Vec::new();
    let trimmed_start = text.len() - text.trim_start().len();
    let mut scan_from = trimmed_start;
    if text[scan_from..].starts_with('`') {
        return words;
    }
    if text[scan_from..].starts_with('/') {
        // Skip slash command itself (e.g. /wh, /onln, /glook, /who).
        if let Some(sp) = text[scan_from..].find(char::is_whitespace) {
            scan_from += sp;
        } else {
            return words;
        }
    }
    let bytes = text.as_bytes();
    let mut i = scan_from;
    while i < bytes.len() {
        while i < bytes.len() && text[i..].chars().next().is_some_and(char::is_whitespace) {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
        let tok_start = i;
        while i < bytes.len() && !text[i..].chars().next().is_some_and(char::is_whitespace) {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
        let tok = &text[tok_start..i];
        if tok.is_empty()
            || tok.starts_with("http://")
            || tok.starts_with("https://")
            || tok.starts_with("furc://")
            || tok.starts_with('@')
            || tok.starts_with('#')
            || tok.starts_with('<')
        {
            continue;
        }
        let mut s = tok_start;
        let mut e = i;
        while s < e {
            let ch = text[s..e].chars().next().unwrap();
            if ch.is_ascii_alphabetic() {
                break;
            }
            s += ch.len_utf8();
        }
        while e > s {
            let ch = text[s..e].chars().next_back().unwrap();
            if ch.is_ascii_alphabetic() {
                break;
            }
            e -= ch.len_utf8();
        }
        let core = &text[s..e];
        if core.len() >= 2
            && !core.chars().any(|c| c.is_ascii_digit())
            && core.chars().all(|c| c.is_ascii_alphabetic() || c == '\'')
        {
            words.push((core.to_owned(), s, e));
        }
    }
    words
}

static BUNDLED_DICT_BZ2: &[u8] = include_bytes!("../../assets/dictionary.txt.bz2");
static BUNDLED_DICT: std::sync::OnceLock<std::collections::HashSet<String>> =
    std::sync::OnceLock::new();

fn bundled_dictionary() -> &'static std::collections::HashSet<String> {
    BUNDLED_DICT.get_or_init(|| {
        use std::io::Read;
        let mut set = std::collections::HashSet::with_capacity(130_000);
        let mut decoder = bzip2_rs::DecoderReader::new(BUNDLED_DICT_BZ2);
        let mut text = String::new();
        if decoder.read_to_string(&mut text).is_ok() {
            for line in text.lines() {
                let w = line.trim();
                if !w.is_empty() {
                    set.insert(w.to_owned());
                }
            }
        }
        set
    })
}

fn spellcheck_word_known(
    word: &str,
    custom_dict: &std::collections::HashSet<String>,
    dict: &std::collections::HashSet<String>,
) -> bool {
    let lower = word.to_ascii_lowercase();
    if custom_dict.contains(&lower) || dict.contains(&lower) {
        return true;
    }
    if let Some(base) = lower.strip_suffix("'s").or_else(|| lower.strip_suffix('\'')) {
        if custom_dict.contains(base) || dict.contains(base) {
            return true;
        }
    }
    false
}

fn edit1_suggestions(
    word: &str,
    dict: &std::collections::HashSet<String>,
    limit: usize,
) -> Vec<String> {
    let lower = word.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push_if_valid = |cand: String| {
        if found.len() < limit && dict.contains(&cand) && seen.insert(cand.clone()) {
            found.push(cand);
        }
    };
    // Deletions
    for i in 0..bytes.len() {
        let mut s = String::with_capacity(bytes.len() - 1);
        s.push_str(&lower[..i]);
        s.push_str(&lower[i + 1..]);
        push_if_valid(s);
    }
    // Transpositions
    for i in 0..bytes.len().saturating_sub(1) {
        let mut b = bytes.to_vec();
        b.swap(i, i + 1);
        if let Ok(s) = String::from_utf8(b) {
            push_if_valid(s);
        }
    }
    // Replacements
    for i in 0..bytes.len() {
        for ch in b'a'..=b'z' {
            if bytes[i] == ch {
                continue;
            }
            let mut b = bytes.to_vec();
            b[i] = ch;
            if let Ok(s) = String::from_utf8(b) {
                push_if_valid(s);
            }
        }
    }
    // Insertions
    for i in 0..=bytes.len() {
        for ch in b'a'..=b'z' {
            let mut b = Vec::with_capacity(bytes.len() + 1);
            b.extend_from_slice(&bytes[..i]);
            b.push(ch);
            b.extend_from_slice(&bytes[i..]);
            if let Ok(s) = String::from_utf8(b) {
                push_if_valid(s);
            }
        }
    }
    found
}

fn run_bundled_spell_check(
    text: &str,
    custom_dict: &std::collections::HashSet<String>,
) -> Vec<MisspelledWord> {
    let dict = bundled_dictionary();
    extract_spellcheck_words(text)
        .into_iter()
        .filter(|(w, _, _)| !spellcheck_word_known(w, custom_dict, dict))
        .map(|(word, start, end)| {
            let suggestions = edit1_suggestions(&word, dict, 5);
            MisspelledWord {
                word,
                start,
                end,
                suggestions,
            }
        })
        .collect()
}

fn run_hunspell_check(
    text: &str,
    custom_dict: &std::collections::HashSet<String>,
) -> Vec<MisspelledWord> {
    let Some(dic) = find_hunspell_dict() else {
        return run_bundled_spell_check(text, custom_dict);
    };
    let dict = bundled_dictionary();
    let words = extract_spellcheck_words(text);
    if words.is_empty() {
        return Vec::new();
    }
    let mut stdin_buf = String::new();
    let mut active = Vec::new();
    for (w, s, e) in words {
        if spellcheck_word_known(&w, custom_dict, dict) {
            continue;
        }
        stdin_buf.push('^');
        stdin_buf.push_str(&w);
        stdin_buf.push('\n');
        active.push((w, s, e));
    }
    if active.is_empty() {
        return Vec::new();
    }
    let mut cmd = furcadia_client::sys::command("hunspell");
    cmd.args(["-d", &dic, "-a"]);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return run_bundled_spell_check(text, custom_dict);
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(stdin_buf.as_bytes());
    }
    let Ok(out) = child.wait_with_output() else {
        return run_bundled_spell_check(text, custom_dict);
    };
    if !out.status.success() {
        return run_bundled_spell_check(text, custom_dict);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut results = Vec::new();
    let mut word_idx = 0usize;
    for line in stdout.lines().skip(1) {
        if line.is_empty() {
            word_idx += 1;
            continue;
        }
        let Some((word, start, end)) = active.get(word_idx) else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("& ") {
            let suggestions = rest
                .split_once(": ")
                .map(|(_, s)| s.split(", ").take(5).map(|x| x.trim().to_owned()).collect())
                .unwrap_or_default();
            results.push(MisspelledWord {
                word: word.clone(),
                start: *start,
                end: *end,
                suggestions,
            });
        } else if line.starts_with("# ") {
            results.push(MisspelledWord {
                word: word.clone(),
                start: *start,
                end: *end,
                suggestions: Vec::new(),
            });
        }
    }
    results
}

fn build_spellcheck_layout_job(
    ui: &egui::Ui,
    text: &str,
    wrap_width: f32,
    misspelled: &[MisspelledWord],
    text_color: Color32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let base_fmt = egui::TextFormat {
        font_id: font_id.clone(),
        color: text_color,
        ..Default::default()
    };
    let err_fmt = egui::TextFormat {
        font_id,
        color: text_color,
        underline: egui::Stroke::new(1.5_f32, Color32::from_rgb(245, 90, 90)),
        ..Default::default()
    };
    let mut cursor = 0usize;
    for m in misspelled {
        if m.start < cursor || m.end > text.len() || !text.is_char_boundary(m.start) || !text.is_char_boundary(m.end) {
            continue;
        }
        if m.start > cursor {
            job.append(&text[cursor..m.start], 0.0, base_fmt.clone());
        }
        job.append(&text[m.start..m.end], 0.0, err_fmt.clone());
        cursor = m.end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, base_fmt);
    }
    job
}

fn read_system_clipboard() -> Option<String> {
    for (bin, args) in [
        ("wl-paste", ["--no-newline"].as_slice()),
        ("xclip", ["-o", "-selection", "clipboard"].as_slice()),
        ("xsel", ["-ob"].as_slice()),
    ] {
        if let Ok(out) = std::process::Command::new(bin).args(args).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn percent_encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn plain_server_text(raw_html: &str) -> String {
    let mut out = String::with_capacity(raw_html.len());
    let mut in_tag = false;
    for ch in raw_html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_owned()
}

fn extract_dialog_notice(payload: &str) -> String {
    // Format: `<dialog> <code> <type> <message>` or `<dialog> <code> <message>`
    let mut parts = payload.splitn(4, ' ');
    let _dialog = parts.next();
    let _code = parts.next();
    let third = parts.next().unwrap_or("");
    let fourth = parts.next().unwrap_or("");
    let combined;
    let raw_notice = if fourth.is_empty() {
        if !third.is_empty() {
            third
        } else {
            payload
        }
    } else if third.chars().all(|c| c.is_ascii_digit() || c == '-')
        || matches!(third, "error" | "ok" | "info" | "warn")
    {
        fourth
    } else {
        combined = format!("{third} {fourth}");
        &combined
    };
    plain_server_text(raw_notice)
}

fn default_connection_config() -> ConnectionConfig {
    ConnectionConfig {
        host: DEFAULT_HOST.to_owned(),
        port: DEFAULT_PORT,
        connect_timeout: Duration::from_secs(8),
        read_timeout: Some(Duration::from_millis(50)),
    }
}

fn forward_pre_handshake_msgs(
    pre_msgs: Vec<ServerMessage>,
    tx: &Sender<NetToUi>,
    last_server_notice: &mut Option<String>,
) {
    for msg in pre_msgs {
        if let ServerMessage::Text(raw) = &msg {
            let txt = plain_server_text(raw);
            if !txt.is_empty() {
                *last_server_notice = Some(txt);
            }
        } else if let ServerMessage::Extended {
            code: Some('#'),
            payload,
        } = &msg
        {
            let txt = extract_dialog_notice(payload);
            if !txt.is_empty() {
                *last_server_notice = Some(txt);
            }
        }
        let _ = tx.send(NetToUi::Message(msg));
    }
}

fn connect_and_retry_pipe_account(
    email: &str,
    character: &str,
    password: &str,
    tx: &Sender<NetToUi>,
    seq: u64,
    last_server_notice: &mut Option<String>,
) -> Option<FurcadiaClient> {
    let pipe_char = character.trim().replace([' ', '_'], "|");
    let _ = tx.send(NetToUi::ConnectStage {
        seq,
        stage: format!("Retrying login as '{pipe_char}'..."),
    });
    let mut c = FurcadiaClient::connect(&default_connection_config()).ok()?;
    let pre_msgs = c.wait_for_handshake_timeout(Duration::from_secs(12)).ok()?;
    forward_pre_handshake_msgs(pre_msgs, tx, last_server_notice);
    let tls_msgs = c.enable_tls(Duration::from_secs(12)).ok()?;
    forward_pre_handshake_msgs(tls_msgs, tx, last_server_notice);
    c.login_account_raw_char(email, &pipe_char, password).ok()?;
    *last_server_notice = None;
    Some(c)
}

/// Background networking loop for a single character session.
///
/// Owns the [`FurcadiaClient`] socket, processes [`UiToNet`] commands, spawns
/// short-lived worker threads for HTTP/file-server downloads and spellchecking,
/// sends 30 s `time` keepalive probes when idle, and exits cleanly once the UI
/// drops its [`Sender<UiToNet>`].
fn network_worker(rx: Receiver<UiToNet>, tx: Sender<NetToUi>, ctx: egui::Context) {
    let mut client: Option<FurcadiaClient> = None;
    let mut active_seq: u64 = 0;
    let mut awaiting_login_ack = false;
    let mut pending_desc: Option<String> = None;
    let mut pending_colors: Option<String> = None;
    let mut login_deadline: Option<Instant> = None;
    let mut last_server_notice: Option<String> = None;
    let mut account_retry: Option<(u64, String, String, String)> = None;
    let mut last_server_activity = Instant::now();
    let mut heartbeat_sent: Option<Instant> = None;

    'worker: loop {
        loop {
            let cmd = match rx.try_recv() {
                Ok(cmd) => cmd,
                Err(TryRecvError::Empty) => break,
                // UI dropped the channel (e.g., closed character tab or app exit):
                // disconnect cleanly and exit the thread so neither socket nor stack leaks.
                Err(TryRecvError::Disconnected) => {
                    if let Some(mut cl) = client.take() {
                        let _ = cl.disconnect();
                    }
                    break 'worker;
                }
            };
            match cmd {
                UiToNet::AuthenticateAccount { email, password } => {
                    match fetch_account_characters(&email, &password) {
                        Ok(info) => {
                            let _ = tx.send(NetToUi::Authenticated(info));
                            ctx.request_repaint();
                            let news_tx = tx.clone();
                            let news_ctx = ctx.clone();
                            thread::spawn(move || {
                                if let Ok(entries) = fetch_news() {
                                    let _ = news_tx.send(NetToUi::News(entries));
                                    news_ctx.request_repaint();
                                }
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(NetToUi::AuthError(err.to_string()));
                            ctx.request_repaint();
                        }
                    }
                }
                UiToNet::ConnectCharacter {
                    seq,
                    email,
                    character,
                    password,
                    updated_desc,
                    colors,
                } => {
                    if let Some(mut old) = client.take() {
                        let _ = old.disconnect();
                    }
                    active_seq = seq;
                    awaiting_login_ack = false;
                    pending_desc = updated_desc;
                    pending_colors = colors;
                    login_deadline = None;
                    last_server_activity = Instant::now();
                    heartbeat_sent = None;
                    last_server_notice = None;
                    account_retry = None;

                    let stage = format!("Connecting to {DEFAULT_HOST}:{DEFAULT_PORT}...");
                    let _ = tx.send(NetToUi::Status(stage.clone()));
                    let _ = tx.send(NetToUi::ConnectStage { seq, stage });
                    ctx.request_repaint();

                    let cfg = default_connection_config();

                    match FurcadiaClient::connect(&cfg) {
                        Ok(mut c) => {
                            let _ = tx.send(NetToUi::ConnectStage {
                                seq,
                                stage: "Waiting for server handshake (Dragonroar)...".into(),
                            });
                            ctx.request_repaint();

                            match c.wait_for_handshake_timeout(Duration::from_secs(12)) {
                                Ok(pre_msgs) => {
                                    forward_pre_handshake_msgs(pre_msgs, &tx, &mut last_server_notice);
                                }
                                Err(err) => {
                                    let msg = format!("Handshake failed: {err}");
                                    let _ = tx.send(NetToUi::Status(msg.clone()));
                                    let _ = tx.send(NetToUi::ConnectFailed { seq, error: msg });
                                    ctx.request_repaint();
                                    continue;
                                }
                            }

                            let _ = tx.send(NetToUi::ConnectStage {
                                seq,
                                stage: "Negotiating encrypted connection...".into(),
                            });
                            ctx.request_repaint();
                            match c.enable_tls(Duration::from_secs(12)) {
                                Ok(tls_msgs) => {
                                    forward_pre_handshake_msgs(
                                        tls_msgs,
                                        &tx,
                                        &mut last_server_notice,
                                    );
                                    // Honor `FurcadiaClient::enable_tls` contract: inform the user
                                    // that Furcadia's legacy TLS server cert is not verified.
                                    let _ = tx.send(NetToUi::Status(
                                        "Legacy TLS active (server certificate not verified)".into(),
                                    ));
                                }
                                Err(err) => {
                                    let msg = format!(
                                        "Secure connection failed; refusing to send credentials: {err}"
                                    );
                                    let _ = tx.send(NetToUi::Status(msg.clone()));
                                    let _ = tx.send(NetToUi::ConnectFailed { seq, error: msg });
                                    ctx.request_repaint();
                                    continue;
                                }
                            }

                            let _ = tx.send(NetToUi::ConnectStage {
                                seq,
                                stage: format!("Authenticating character '{character}'..."),
                            });
                            ctx.request_repaint();

                            let retry_candidate =
                                if !email.trim().is_empty() && character.contains([' ', '|']) {
                                    Some((seq, email.clone(), character.clone(), password.clone()))
                                } else {
                                    None
                                };
                            let login_res = if email.trim().is_empty() {
                                c.login_character(character, password)
                            } else {
                                c.login_account(email, character, password)
                            };
                            if let Err(err) = login_res {
                                let msg = format!("Login failed: {err}");
                                let _ = tx.send(NetToUi::Status(msg.clone()));
                                let _ = tx.send(NetToUi::ConnectFailed { seq, error: msg });
                                ctx.request_repaint();
                                continue;
                            }

                            account_retry = retry_candidate;
                            awaiting_login_ack = true;
                            login_deadline = Some(Instant::now() + Duration::from_secs(15));
                            client = Some(c);
                            ctx.request_repaint();
                        }
                        Err(err) => {
                            let msg = format!(
                                "Unable to connect to {DEFAULT_HOST}:{DEFAULT_PORT}: {err}"
                            );
                            let _ = tx.send(NetToUi::Status(msg.clone()));
                            let _ = tx.send(NetToUi::ConnectFailed { seq, error: msg });
                            ctx.request_repaint();
                        }
                    }
                }
                UiToNet::Send(c) => {
                    if let Some(cl) = client.as_mut() {
                        if let Err(err) = cl.send_command(&c) {
                            client = None;
                            let _ = tx.send(NetToUi::Connected {
                                seq: active_seq,
                                connected: false,
                            });
                            let _ = tx.send(NetToUi::Status(format!(
                                "Connection lost while sending command: {err}"
                            )));
                            ctx.request_repaint();
                        }
                    }
                }
                UiToNet::DownloadDream {
                    url,
                    map_name,
                    checksum,
                } => {
                    let dl_tx = tx.clone();
                    let dl_ctx = ctx.clone();
                    thread::spawn(move || {
                        let Some(dreams_dir) = AssetLocator::dreams_cache_dir() else {
                            let _ = dl_tx.send(NetToUi::DreamError(
                                "Could not determine dream cache directory.".into(),
                            ));
                            dl_ctx.request_repaint();
                            return;
                        };
                        let _ = std::fs::create_dir_all(&dreams_dir);
                        let safe_name = FurcadiaApp::sanitize_dream_id(&map_name);
                        // Guard against empty sanitized name: `dreams_dir.join("")` would
                        // resolve to `dreams_dir` itself and wipe the entire dreams cache.
                        if safe_name.is_empty() {
                            let _ = dl_tx.send(NetToUi::DreamError(format!(
                                "Refusing to download dream with invalid map name '{map_name}'."
                            )));
                            dl_ctx.request_repaint();
                            return;
                        }
                        let suffix = format!("{:?}", std::thread::current().id())
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric())
                            .collect::<String>();
                        let dl_path = dreams_dir.join(format!("{safe_name}_{suffix}.tmp"));
                        let hdr_path = dreams_dir.join(format!("{safe_name}_{suffix}.hdr"));
                        let _ = std::fs::remove_file(&dl_path);
                        let _ = std::fs::remove_file(&hdr_path);

                        let mut child = match furcadia_client::sys::command("curl")
                            .args([
                                "-fsSL",
                                "--proto",
                                "=http,https",
                                "--proto-redir",
                                "=http,https",
                                "--max-filesize",
                                "67108864",
                                "--connect-timeout",
                                "15",
                                "--speed-time",
                                "30",
                                "--speed-limit",
                                "512",
                                "--max-time",
                                "300",
                                "-D",
                            ])
                            .arg(&hdr_path)
                            .arg("-o")
                            .arg(&dl_path)
                            .arg("--")
                            .arg(&url)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::piped())
                            .spawn()
                        {
                            Ok(c) => c,
                            Err(e) => {
                                let _ = dl_tx.send(NetToUi::DreamError(format!(
                                    "Failed to launch curl for '{map_name}': {e}"
                                )));
                                dl_ctx.request_repaint();
                                return;
                            }
                        };

                        let mut total_bytes: Option<u64> = None;
                        let mut last_reported_bytes: u64 = 0;
                        let status = loop {
                            if total_bytes.is_none() {
                                if let Ok(hdr_text) = std::fs::read_to_string(&hdr_path) {
                                    total_bytes = parse_http_content_length(&hdr_text);
                                }
                            }
                            let cur_bytes = std::fs::metadata(&dl_path).map(|m| m.len()).unwrap_or(0);
                            if cur_bytes != last_reported_bytes || total_bytes.is_some() {
                                last_reported_bytes = cur_bytes;
                                let _ = dl_tx.send(NetToUi::DreamDownloadProgress {
                                    map_name: map_name.clone(),
                                    received_bytes: cur_bytes,
                                    total_bytes,
                                    stage: if cur_bytes > 0 {
                                        "Downloading dream package...".to_owned()
                                    } else {
                                        "Connecting to dream server...".to_owned()
                                    },
                                });
                                dl_ctx.request_repaint();
                            }
                            match child.try_wait() {
                                Ok(Some(st)) => break Ok(st),
                                Ok(None) => thread::sleep(Duration::from_millis(80)),
                                Err(e) => break Err(e),
                            }
                        };
                        let _ = std::fs::remove_file(&hdr_path);

                        match status {
                            Ok(st) if st.success() => {}
                            Ok(_) => {
                                let mut err = String::new();
                                if let Some(mut stderr) = child.stderr.take() {
                                    use std::io::Read;
                                    let _ = stderr.read_to_string(&mut err);
                                }
                                let err = err.trim().to_owned();
                                let _ = std::fs::remove_file(&dl_path);
                                let _ = dl_tx.send(NetToUi::DreamError(format!(
                                    "Dream download failed for '{map_name}' ({url}): {err}"
                                )));
                                dl_ctx.request_repaint();
                                return;
                            }
                            Err(e) => {
                                let _ = std::fs::remove_file(&dl_path);
                                let _ = dl_tx.send(NetToUi::DreamError(format!(
                                    "Failed while waiting for curl for '{map_name}': {e}"
                                )));
                                dl_ctx.request_repaint();
                                return;
                            }
                        }

                        let bytes = match std::fs::read(&dl_path) {
                            Ok(b) => b,
                            Err(e) => {
                                let _ = dl_tx.send(NetToUi::DreamError(format!(
                                    "Failed to read downloaded dream '{map_name}': {e}"
                                )));
                                dl_ctx.request_repaint();
                                return;
                            }
                        };
                        let final_len = bytes.len() as u64;
                        let _ = dl_tx.send(NetToUi::DreamDownloadProgress {
                            map_name: map_name.clone(),
                            received_bytes: final_len,
                            total_bytes: Some(final_len),
                            stage: "Extracting dream assets...".to_owned(),
                        });
                        dl_ctx.request_repaint();
                        let out_dir = dreams_dir.join(&safe_name);
                        let stage_dir = dreams_dir.join(format!("{safe_name}_{suffix}_extract"));
                        let _ = std::fs::remove_dir_all(&stage_dir);

                        let extracted = if is_fr01(&bytes) {
                            extract_fr01_to_dir(&bytes, &stage_dir)
                                .map(|_| true)
                                .map_err(|e| format!("Failed to extract FR01 dream '{map_name}': {e}"))
                        } else if bytes.starts_with(b"PK") {
                            let _ = std::fs::create_dir_all(&stage_dir);
                            extract_zip_archive(&dl_path, &stage_dir)
                                .map(|()| true)
                                .map_err(|e| format!("Failed to unzip dream '{map_name}': {e}"))
                        } else if bytes.starts_with(b"MAP ") {
                            let map_path = dreams_dir.join(format!("{safe_name}.map"));
                            std::fs::rename(&dl_path, &map_path)
                                .map(|_| false)
                                .map_err(|e| format!("Failed to store dream map '{map_name}': {e}"))
                        } else {
                            let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(80)])
                                .trim()
                                .to_owned();
                            Err(format!(
                                "Unknown dream package format for '{map_name}' ({} bytes from {url}: {preview}).",
                                bytes.len()
                            ))
                        };

                        let _ = std::fs::remove_file(&dl_path);
                        match extracted {
                            Ok(true) => {
                                let _ = std::fs::remove_dir_all(&out_dir);
                                if let Err(e) = std::fs::rename(&stage_dir, &out_dir) {
                                    let _ = std::fs::remove_dir_all(&stage_dir);
                                    let _ = dl_tx.send(NetToUi::DreamError(format!(
                                        "Failed to finalize dream directory '{map_name}': {e}"
                                    )));
                                    dl_ctx.request_repaint();
                                    return;
                                }
                                if let Some(hash) = checksum.as_deref() {
                                    let _ = std::fs::write(out_dir.join(".hash"), hash);
                                }
                                if let Some(map_path) = FurcadiaApp::find_map_in_dir(&out_dir) {
                                    let _ = dl_tx.send(NetToUi::DreamDownloaded {
                                        map_name,
                                        path: map_path,
                                        dream_dir: Some(out_dir),
                                    });
                                } else {
                                    let _ = dl_tx.send(NetToUi::DreamError(format!(
                                        "Dream package '{map_name}' contained no .map file."
                                    )));
                                }
                            }
                            Ok(false) => {
                                let _ = std::fs::remove_dir_all(&stage_dir);
                                let map_path = dreams_dir.join(format!("{safe_name}.map"));
                                let _ = dl_tx.send(NetToUi::DreamDownloaded {
                                    map_name,
                                    path: map_path,
                                    dream_dir: None,
                                });
                            }
                            Err(msg) => {
                                let _ = std::fs::remove_dir_all(&stage_dir);
                                let _ = dl_tx.send(NetToUi::DreamError(msg));
                            }
                        }
                        dl_ctx.request_repaint();
                    });
                }
                UiToNet::DownloadStockPatch { slot } => {
                    let dl_tx = tx.clone();
                    let dl_ctx = ctx.clone();
                    thread::spawn(move || {
                        let subdir = slot.subdir();
                        let fname = slot.filename();
                        let archive = AssetLocator::stock_patch_cache_path(subdir, &fname)
                            .and_then(|dest_path| {
                                if dest_path.is_file() {
                                    if let Ok(arc) = FoxArchive::load(&dest_path) {
                                        return Some(Box::new(arc));
                                    }
                                }
                                let parent = dest_path.parent()?;
                                std::fs::create_dir_all(parent).ok()?;
                                let suffix = format!("{:?}", std::thread::current().id())
                                    .chars()
                                    .filter(|c| c.is_ascii_alphanumeric())
                                    .collect::<String>();
                                let tmp_path = parent.join(format!("{fname}.{suffix}.tmp"));
                                let _ = std::fs::remove_file(&tmp_path);
                                let url = AssetLocator::stock_patch_url(subdir, &fname);
                                let curl_ok = furcadia_client::sys::command("curl")
                                    .args([
                                        "-fsSL",
                                        "--proto",
                                        "=https",
                                        "--proto-redir",
                                        "=https",
                                        "--max-filesize",
                                        "67108864",
                                        "--connect-timeout",
                                        "15",
                                        "--speed-time",
                                        "30",
                                        "--speed-limit",
                                        "512",
                                        "--max-time",
                                        "300",
                                        "-o",
                                    ])
                                    .arg(&tmp_path)
                                    .arg("--")
                                    .arg(&url)
                                    .output()
                                    .is_ok_and(|o| o.status.success());
                                if !curl_ok || !tmp_path.is_file() {
                                    let _ = std::fs::remove_file(&tmp_path);
                                    return None;
                                }
                                let Ok(arc) = FoxArchive::load(&tmp_path) else {
                                    let _ = std::fs::remove_file(&tmp_path);
                                    return None;
                                };
                                if replace_file_atomically(&tmp_path, &dest_path).is_err() {
                                    let _ = std::fs::remove_file(&tmp_path);
                                    return None;
                                }
                                Some(Box::new(arc))
                            });
                        let _ = dl_tx.send(NetToUi::StockPatchDownloaded { slot, archive });
                        dl_ctx.request_repaint();
                    });
                }
                UiToNet::DownloadSpecies {
                    file_id,
                    server_code,
                    if_newer_than,
                } => {
                    let dl_tx = tx.clone();
                    let dl_ctx = ctx.clone();
                    thread::spawn(move || {
                        let suffix = format!("{:?}", std::thread::current().id())
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric())
                            .collect::<String>();
                        let is_dynamic = file_id >= DYNAMIC_AVATAR_CACHE_BASE;
                        if !is_dynamic {
                            // Stock species (`speciesN.fox`, 1..1000) and local species
                            // (`localN.fox`, 1001..1004) come from the official web-client asset
                            // host. NOTE: `apollo.../species/AvatarN.zip` is the *dynamic avatar*
                            // `DPlayerN.fox`, a different namespace — never use it for stock species.
                            let fname = if file_id > 1000 {
                                format!("local{}.fox", file_id - 1000)
                            } else {
                                format!("species{file_id}.fox")
                            };
                            let final_path = AssetLocator::stock_species_cache_dir().and_then(|dir| {
                                std::fs::create_dir_all(&dir).ok()?;
                                let dest_path = dir.join(&fname);
                                let tmp_path = dir.join(format!("{fname}.{suffix}.tmp"));
                                let _ = std::fs::remove_file(&tmp_path);
                                let mut cmd = furcadia_client::sys::command("curl");
                                cmd.args(["-fsSL", "--max-time", "25"]);
                                if let Some(ref existing) = if_newer_than {
                                    if existing.is_file() {
                                        cmd.arg("-z").arg(existing);
                                    }
                                }
                                let curl_ok = cmd
                                    .arg("-o")
                                    .arg(&tmp_path)
                                    .arg(AssetLocator::stock_species_url(&fname))
                                    .output()
                                    .is_ok_and(|o| o.status.success());
                                let valid = curl_ok
                                    && tmp_path.is_file()
                                    && FoxArchive::load(&tmp_path).is_ok();
                                if !valid {
                                    let _ = std::fs::remove_file(&tmp_path);
                                    return None;
                                }
                                replace_file_atomically(&tmp_path, &dest_path).ok()?;
                                Some(dest_path)
                            });
                            let _ = dl_tx.send(NetToUi::SpeciesDownloaded {
                                file_id,
                                path: final_path,
                            });
                            dl_ctx.request_repaint();
                            return;
                        }
                        let Some(species_dir) = AssetLocator::species_cache_dir() else {
                            let _ = dl_tx.send(NetToUi::SpeciesDownloaded {
                                file_id,
                                path: None,
                            });
                            dl_ctx.request_repaint();
                            return;
                        };
                        let _ = std::fs::create_dir_all(&species_dir);
                        let avatar_id = file_id - DYNAMIC_AVATAR_CACHE_BASE;
                        let dest_name = format!("dplayer{avatar_id}.fox");
                        let dest_path = species_dir.join(&dest_name);
                        let srv = server_code
                            .filter(|c| *c != ' ' && c.is_ascii_alphanumeric())
                            .map(|c| c.to_string())
                            .unwrap_or_default();
                        let mut final_path = None;

                        // 1. Try direct DPlayer<id>.fox first (matches official web client).
                        let direct_tmp = species_dir.join(format!("dplayer{avatar_id}_{suffix}.tmp"));
                        let _ = std::fs::remove_file(&direct_tmp);
                        let direct_url = format!(
                            "https://apollo.furcadia.com/species{srv}/DPlayer{avatar_id}.fox"
                        );
                        let mut direct_cmd = furcadia_client::sys::command("curl");
                        direct_cmd.args(["-fsSL", "--max-time", "25"]);
                        let has_existing = if_newer_than.as_ref().is_some_and(|p| p.is_file());
                        if let Some(ref existing) = if_newer_than {
                            if existing.is_file() {
                                direct_cmd.arg("-z").arg(existing);
                            }
                        }
                        let direct_ok = direct_cmd
                            .arg("-o")
                            .arg(&direct_tmp)
                            .arg(&direct_url)
                            .output()
                            .is_ok_and(|o| o.status.success());
                        if direct_ok && has_existing && !direct_tmp.is_file() {
                            // HTTP 304 Not Modified: keep cached file and skip .zip fallback.
                            let _ = std::fs::remove_file(&direct_tmp);
                            let _ = dl_tx.send(NetToUi::SpeciesDownloaded {
                                file_id,
                                path: None,
                            });
                            dl_ctx.request_repaint();
                            return;
                        }
                        if direct_ok
                            && direct_tmp.is_file()
                            && FoxArchive::load(&direct_tmp).is_ok()
                        {
                            if replace_file_atomically(&direct_tmp, &dest_path).is_ok() {
                                final_path = Some(dest_path.clone());
                            }
                        } else {
                            let _ = std::fs::remove_file(&direct_tmp);
                            // 2. Fallback to Avatar<id>.zip if direct .fox failed or was invalid.
                            let zip_path =
                                species_dir.join(format!("Avatar{avatar_id}_{suffix}.zip.tmp"));
                            let stage_dir = species_dir.join(format!("stage_{file_id}_{suffix}"));
                            let _ = std::fs::remove_file(&zip_path);
                            let _ = std::fs::remove_dir_all(&stage_dir);
                            let url = format!(
                                "https://apollo.furcadia.com/species{srv}/Avatar{avatar_id}.zip"
                            );
                            let mut cmd = furcadia_client::sys::command("curl");
                            cmd.args(["-fsSL", "--max-time", "25"]);
                            if let Some(ref existing) = if_newer_than {
                                if existing.is_file() {
                                    cmd.arg("-z").arg(existing);
                                }
                            }
                            let curl_ok = cmd
                                .arg("-o")
                                .arg(&zip_path)
                                .arg(&url)
                                .output()
                                .is_ok_and(|o| o.status.success());
                            if curl_ok && zip_path.is_file() {
                                let _ = std::fs::create_dir_all(&stage_dir);
                                let unzip_ok = extract_zip_archive(&zip_path, &stage_dir).is_ok();
                                if unzip_ok {
                                    let found = FurcadiaApp::find_file_case_insensitive(
                                        &stage_dir,
                                        &dest_name,
                                    )
                                    .or_else(|| {
                                        std::fs::read_dir(&stage_dir).ok().and_then(|rd| {
                                            rd.flatten()
                                                .map(|e| e.path())
                                                .find(|p| {
                                                    p.extension()
                                                        .and_then(|s| s.to_str())
                                                        .is_some_and(|e| {
                                                            e.eq_ignore_ascii_case("fox")
                                                        })
                                                })
                                        })
                                    });
                                    if let Some(src) = found {
                                        if FoxArchive::load(&src).is_ok()
                                            && replace_file_atomically(&src, &dest_path).is_ok()
                                        {
                                            final_path = Some(dest_path);
                                        }
                                    }
                                }
                            }
                            let _ = std::fs::remove_file(&zip_path);
                            let _ = std::fs::remove_dir_all(&stage_dir);
                        }
                        let _ = std::fs::remove_file(&direct_tmp);
                        let _ = dl_tx.send(NetToUi::SpeciesDownloaded {
                            file_id,
                            path: final_path,
                        });
                        dl_ctx.request_repaint();
                    });
                }
                UiToNet::DownloadPortrait { portrait_id } => {
                    let dl_tx = tx.clone();
                    let dl_ctx = ctx.clone();
                    thread::spawn(move || {
                        // Recolorable portrait pixels are encoded through Furcadia's
                        // palette. A grayscale fallback destroys those channel values.
                        let palette = AssetLocator::discover()
                            .load_default_palette()
                            .unwrap_or_default();
                        let cache_path = AssetLocator::species_cache_dir()
                            .map(|d| d.join(format!("pp{portrait_id}.fprt")));
                        let cached_bytes = cache_path
                            .as_ref()
                            .and_then(|p| std::fs::read(p).ok())
                            .filter(|b| !b.is_empty());
                        let decoded = cached_bytes
                            .and_then(|bytes| decode_furcadia_portrait(&bytes, &palette))
                            .or_else(|| {
                                let fname = format!("pp{portrait_id}");
                                let bytes = download_furcadia_file_server(&fname)?;
                                let decoded = decode_furcadia_portrait(&bytes, &palette)?;
                                if let Some(ref p) = cache_path {
                                    if let Some(parent) = p.parent() {
                                        let _ = std::fs::create_dir_all(parent);
                                    }
                                    let _ = std::fs::write(p, &bytes);
                                }
                                Some(decoded)
                            });
                        let (rgba, remappable) = decoded
                            .map(|(w, h, rgba, remappable)| (Some((w, h, rgba)), remappable))
                            .unwrap_or((None, false));
                        let _ = dl_tx.send(NetToUi::PortraitDownloaded { portrait_id, rgba, remappable });
                        dl_ctx.request_repaint();
                    });
                }
                UiToNet::DownloadChatImage { url } => {
                    let dl_tx = tx.clone();
                    let dl_ctx = ctx.clone();
                    thread::spawn(move || {
                        let mut h: u64 = 0xcbf29ce484222325;
                        for &b in url.as_bytes() {
                            h ^= b as u64;
                            h = h.wrapping_mul(0x100000001b3);
                        }
                        let cache_path = AssetLocator::species_cache_dir()
                            .map(|d| d.join(format!("chat_img_{h:016x}.bin")));
                        let raw = cache_path
                            .as_ref()
                            .and_then(|p| std::fs::read(p).ok())
                            .filter(|b| !b.is_empty())
                            .or_else(|| {
                                let out = furcadia_client::sys::command("curl")
                                    .args([
                                        "-fsSL",
                                        "--proto",
                                        "=http,https",
                                        "--proto-redir",
                                        "=http,https",
                                        "--max-filesize",
                                        "10485760",
                                        "--max-time",
                                        "10",
                                        "--",
                                    ])
                                    .arg(&url)
                                    .output()
                                    .ok()
                                    .filter(|o| o.status.success() && !o.stdout.is_empty())?;
                                if let Some(ref p) = cache_path {
                                    if let Some(parent) = p.parent() {
                                        let _ = std::fs::create_dir_all(parent);
                                    }
                                    let _ = std::fs::write(p, &out.stdout);
                                }
                                Some(out.stdout)
                            });
                        let rgba = raw
                            .and_then(|bytes| image::load_from_memory(&bytes).ok())
                            .map(|img| {
                                let rgba = img.to_rgba8();
                                let (w, h) = rgba.dimensions();
                                (w as usize, h as usize, rgba.into_raw())
                            });
                        let _ = dl_tx.send(NetToUi::ChatImageDownloaded { url, rgba });
                        dl_ctx.request_repaint();
                    });
                }
                UiToNet::SpellCheck {
                    seq,
                    text,
                    custom_dict,
                } => {
                    let sc_tx = tx.clone();
                    let sc_ctx = ctx.clone();
                    thread::spawn(move || {
                        let misspelled = run_hunspell_check(&text, &custom_dict);
                        let _ = sc_tx.send(NetToUi::SpellCheckResult { seq, misspelled });
                        sc_ctx.request_repaint();
                    });
                }
                UiToNet::Disconnect => {
                    if let Some(mut cl) = client.take() {
                        let _ = cl.disconnect();
                    }
                    awaiting_login_ack = false;
                    pending_desc = None;
                    pending_colors = None;
                    login_deadline = None;
                    last_server_notice = None;
                    let _ = tx.send(NetToUi::Connected {
                        seq: active_seq,
                        connected: false,
                    });
                    let _ = tx.send(NetToUi::Status("Disconnected".into()));
                    ctx.request_repaint();
                }
            }
        }

        if let Some(cl) = client.as_mut() {
            match cl.poll_message() {
                Ok(Some(msg)) => {
                    last_server_activity = Instant::now();
                    heartbeat_sent = None;
                    if awaiting_login_ack {
                        match &msg {
                            ServerMessage::LoginAck => {
                                awaiting_login_ack = false;
                                login_deadline = None;
                                account_retry = None;
                                if let Some(desc) = pending_desc.take() {
                                    let _ = cl.send_command(&ClientCommand::Desc(desc));
                                }
                                if let Some(colors) = pending_colors.take() {
                                    let _ = cl.send_command(&ClientCommand::Color(colors));
                                }
                                let _ = tx.send(NetToUi::Connected {
                                    seq: active_seq,
                                    connected: true,
                                });
                                let _ = tx.send(NetToUi::Status("Connected".into()));
                            }
                            ServerMessage::Text(raw) => {
                                let txt = plain_server_text(raw);
                                if !txt.is_empty() {
                                    last_server_notice = Some(txt);
                                }
                            }
                            ServerMessage::Extended {
                                code: Some('#'),
                                payload,
                            } => {
                                let txt = extract_dialog_notice(payload);
                                if !txt.is_empty() {
                                    last_server_notice = Some(txt);
                                }
                            }
                            _ => {}
                        }
                    } else if matches!(cl.state(), SessionState::Connected) {
                        let _ = tx.send(NetToUi::Status("Connected".into()));
                    }
                    let _ = tx.send(NetToUi::Message(msg));
                    ctx.request_repaint();
                }
                Ok(None) => {
                    if awaiting_login_ack && login_deadline.is_some_and(|d| Instant::now() >= d) {
                        client = None;
                        awaiting_login_ack = false;
                        pending_desc = None;
                        login_deadline = None;
                        let err_msg = last_server_notice.take().unwrap_or_else(|| {
                            "Timed out waiting for Furcadia login confirmation.".to_owned()
                        });
                        let _ = tx.send(NetToUi::Status(err_msg.clone()));
                        let _ = tx.send(NetToUi::ConnectFailed {
                            seq: active_seq,
                            error: err_msg,
                        });
                        ctx.request_repaint();
                    } else if !awaiting_login_ack {
                        if let Some(sent) = heartbeat_sent {
                            if sent.elapsed() >= Duration::from_secs(12) {
                                client = None;
                                let _ = tx.send(NetToUi::Connected { seq: active_seq, connected: false });
                                let _ = tx.send(NetToUi::Status("Connection lost (server stopped responding to keepalive).".into()));
                                ctx.request_repaint();
                            }
                        } else if last_server_activity.elapsed() >= Duration::from_secs(30) {
                            let probe_sent = client.as_mut().is_some_and(|cl| {
                                cl.send_command(&ClientCommand::Raw("time".into())).is_ok()
                            });
                            if !probe_sent {
                                    client = None;
                                    let _ = tx.send(NetToUi::Connected { seq: active_seq, connected: false });
                                    let _ = tx.send(NetToUi::Status("Connection lost while checking server response.".into()));
                            } else {
                                heartbeat_sent = Some(Instant::now());
                            }
                            ctx.request_repaint();
                        }
                    }
                }
                Err(err) => {
                    client = None;
                    if awaiting_login_ack {
                        if let Some((retry_seq, retry_email, retry_char, retry_pass)) =
                            account_retry.take()
                        {
                            if retry_seq == active_seq {
                                if let Some(retry_client) = connect_and_retry_pipe_account(
                                    &retry_email,
                                    &retry_char,
                                    &retry_pass,
                                    &tx,
                                    retry_seq,
                                    &mut last_server_notice,
                                ) {
                                    client = Some(retry_client);
                                    login_deadline = Some(Instant::now() + Duration::from_secs(15));
                                    ctx.request_repaint();
                                    continue;
                                }
                            }
                        }
                        awaiting_login_ack = false;
                        pending_desc = None;
                        login_deadline = None;
                        let err_msg = last_server_notice
                            .take()
                            .unwrap_or_else(|| format!("Login rejected or connection closed: {err}"));
                        let _ = tx.send(NetToUi::Status(err_msg.clone()));
                        let _ = tx.send(NetToUi::ConnectFailed {
                            seq: active_seq,
                            error: err_msg,
                        });
                    } else {
                        let _ = tx.send(NetToUi::Connected {
                            seq: active_seq,
                            connected: false,
                        });
                        let _ = tx.send(NetToUi::Status("Disconnected by server".into()));
                    }
                    ctx.request_repaint();
                }
            }
        } else {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn read_line_cr_lf(stream: &mut std::net::TcpStream) -> Option<String> {
    use std::io::Read;
    let mut line = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        if stream.read_exact(&mut byte).is_err() || line.len() > 4096 {
            return None;
        }
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] != b'\r' {
            line.push(byte[0]);
        }
    }
    Some(String::from_utf8_lossy(&line).into_owned())
}

/// Downloads a file (such as a custom portrait `pp<id>`) from Furcadia's file server (`hades.furcadia.com:6500`).
fn download_furcadia_file_server(filename: &str) -> Option<Vec<u8>> {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    let mut stream = None;
    for port in [6500u16, 5000u16] {
        let Ok(addrs) = ("hades.furcadia.com", port).to_socket_addrs() else {
            continue;
        };
        for addr in addrs {
            if let Ok(s) = TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
                stream = Some(s);
                break;
            }
        }
        if stream.is_some() {
            break;
        }
    }
    let mut stream = stream?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let banner = read_line_cr_lf(&mut stream)?;
    if !banner.starts_with("10") {
        return None;
    }
    stream.write_all(b"USER furcdown\n").ok()?;
    let uresp = read_line_cr_lf(&mut stream)?;
    if !uresp.starts_with("20") {
        return None;
    }
    stream.write_all(b"PASS furcdown\n").ok()?;
    let presp = read_line_cr_lf(&mut stream)?;
    if !presp.starts_with("30") {
        return None;
    }

    let req = format!("RC furcdown furcdown {filename} 0 10 0\n");
    stream.write_all(req.as_bytes()).ok()?;
    let rc_resp = read_line_cr_lf(&mut stream)?;
    let mut parts = rc_resp.split_whitespace();
    if parts.next()? != "44" {
        return None;
    }
    let total_size: usize = parts.next()?.parse().ok()?;
    let _window: usize = parts.next()?.parse().ok()?;
    let pkt_size: usize = parts.next()?.parse().ok()?;
    if total_size == 0 || total_size > 8 * 1024 * 1024 || pkt_size <= 10 || pkt_size > 8192 {
        return None;
    }

    let payload_per_pkt = pkt_size - 10;
    let n_pkts = total_size.div_ceil(payload_per_pkt);
    let mut out = Vec::with_capacity(total_size);
    let mut pkt_buf = vec![0u8; pkt_size];
    stream.write_all(b"PKOK 0\n").ok()?;

    for pkt_idx in 0..n_pkts {
        if stream.read_exact(&mut pkt_buf).is_err() {
            let ack = format!("PKOK {pkt_idx}\n");
            let _ = stream.write_all(ack.as_bytes());
            stream.read_exact(&mut pkt_buf).ok()?;
        }
        if &pkt_buf[..2] != b"SC" {
            return None;
        }
        let take = payload_per_pkt.min(total_size - out.len());
        out.extend_from_slice(&pkt_buf[10..10 + take]);
    }
    let _ = stream.write_all(b"QUIT\n");
    Some(out)
}

/// Decompresses Furcadia's bitwise-inverted MSB-first LZW stream (`FPRT10`).
fn decompress_furcadia_lzw(src: &[u8]) -> Option<Vec<u8>> {
    const CLEAR: u16 = 0x100;
    const EOF: u16 = 0x101;
    const MAX_TABLE: usize = 4096;

    let mut pos = 0usize;
    let mut bitbuf: u32 = 0;
    let mut bitlen: u32 = 0;
    let mut nbits: u32 = 9;

    let mut next_code = |nbits: u32| -> u16 {
        while bitlen < nbits {
            if pos >= src.len() {
                return EOF;
            }
            let b = (src[pos] ^ 0xff) as u32;
            pos += 1;
            bitbuf = (bitbuf << 8) | b;
            bitlen += 8;
        }
        bitlen -= nbits;
        let code = (bitbuf >> bitlen) as u16;
        bitbuf &= (1u32 << bitlen).wrapping_sub(1);
        code
    };

    let mut prefix = [0u16; MAX_TABLE];
    let mut suffix = [0u8; MAX_TABLE];
    for (i, slot) in suffix.iter_mut().enumerate().take(256) {
        *slot = i as u8;
    }

    let mut code = next_code(nbits);
    if code == CLEAR {
        code = next_code(nbits);
    }
    if code == EOF || code >= 256 {
        return None;
    }

    let mut out = Vec::with_capacity(95 * 95 * 3);
    out.push(code as u8);
    let mut old_code = code;
    let mut first_char = code as u8;
    let mut next_id: u16 = 0x102;
    let mut stack = Vec::with_capacity(64);

    loop {
        code = next_code(nbits);
        if code == EOF {
            break;
        }
        if code == CLEAR {
            next_id = 0x102;
            nbits = 9;
            code = next_code(nbits);
            if code == EOF || code >= 256 {
                break;
            }
            out.push(code as u8);
            old_code = code;
            first_char = code as u8;
            continue;
        }

        let in_code = code;
        stack.clear();
        if code == next_id {
            stack.push(first_char);
            code = old_code;
        } else if code > next_id {
            return None;
        }

        let mut cur = code as usize;
        while cur >= 256 {
            if stack.len() >= MAX_TABLE {
                return None;
            }
            stack.push(suffix[cur]);
            cur = prefix[cur] as usize;
        }
        first_char = suffix[cur];
        stack.push(first_char);
        for &b in stack.iter().rev() {
            if out.len() >= MAX_LZW_OUTPUT_BYTES {
                return None;
            }
            out.push(b);
        }

        if (next_id as usize) < MAX_TABLE {
            prefix[next_id as usize] = old_code;
            suffix[next_id as usize] = first_char;
            next_id += 1;
            // Furcadia's FPRT10/FPRT01 encoder increments `nbits` to 13 when `next_id`
            // reaches 4095 (`4095 + 1 == 1 << 12`) and writes the table-full `CLEAR`
            // code at 13 bits before resetting back to 9 bits. Capping at 12 desyncs
            // the bitstream on real 95x95 custom portraits.
            if nbits < 13 && (next_id as u32) + 1 == (1u32 << nbits) {
                nbits += 1;
            }
        }
        old_code = in_code;
    }

    Some(out)
}

fn replace_file_atomically(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    if dst.exists() {
        let _ = std::fs::remove_file(dst);
        if std::fs::rename(src, dst).is_ok() {
            return Ok(());
        }
    }
    std::fs::copy(src, dst)?;
    let _ = std::fs::remove_file(src);
    Ok(())
}

fn parse_http_content_length(headers: &str) -> Option<u64> {
    let mut last = None;
    for line in headers.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                if let Ok(len) = v.trim().parse::<u64>() {
                    if len > 0 {
                        last = Some(len);
                    }
                }
            }
        }
    }
    last
}

fn extract_zip_archive(zip_path: &std::path::Path, dest_dir: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let out = furcadia_client::sys::command("tar")
        .arg("-xf")
        .arg(zip_path)
        .arg("-C")
        .arg(dest_dir)
        .output();
    #[cfg(not(target_os = "windows"))]
    let out = furcadia_client::sys::command("unzip")
        .args(["-oq"])
        .arg(zip_path)
        .arg("-d")
        .arg(dest_dir)
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

fn decompress_bzip2(src: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let decoder = bzip2_rs::DecoderReader::new(src);
    let mut limited = decoder.take((MAX_BZIP2_OUTPUT_BYTES + 1) as u64);
    let mut out = Vec::new();
    limited.read_to_end(&mut out).ok()?;
    if out.is_empty() || out.len() > MAX_BZIP2_OUTPUT_BYTES {
        return None;
    }
    Some(out)
}

/// Recognizes Furcadia's encoded portrait remap pixels after palette expansion.
/// The channel marker is `B == 0`, with `G` selecting a remap LUT (1..=47).
/// Requiring a substantial cluster avoids treating a few ordinary dark-red pixels
/// in a full-color portrait as a remapping mask.
fn portrait_has_remap_pixels(rgba: &[u8]) -> bool {
    let mut encoded = 0usize;
    let mut opaque = 0usize;
    for px in rgba.as_chunks::<4>().0 {
        if px[3] == 0 {
            continue;
        }
        opaque += 1;
        if px[2] == 0 && (1..48).contains(&px[1]) {
            encoded += 1;
        }
    }
    encoded >= 64 && encoded.saturating_mul(100) >= opaque.saturating_mul(5)
}

/// Decodes a Furcadia portrait (`FPRT01`, legacy wrappers, or standard image bytes).
fn decode_furcadia_portrait(data: &[u8], palette: &Palette) -> Option<(usize, usize, Vec<u8>, bool)> {
    if let Ok(img) = image::load_from_memory(data) {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        return Some((w as usize, h as usize, rgba.into_raw(), false));
    }
    if data.len() < 9 {
        return None;
    }
    let header = &data[..6];
    let w = data[6] as usize;
    let h = data[7] as usize;
    let fmt = data[8];
    if w == 0 || h == 0 {
        return None;
    }

    // FPRT01 wraps a classic FSH portrait in Furcadia's inverted LZW stream.
    // Its flags byte uses bit 0 to identify portraits designed for color remapping.
    if header == b"FPRT01" {
        let fsh = decompress_furcadia_lzw(&data[9..])?;
        let active_pal = if fmt & 0x01 != 0 {
            palette.with_classic_remap_markers()
        } else {
            palette.clone()
        };
        if let Ok(mut archive) = FoxArchive::from_bytes(fsh.clone()) {
            if let Ok(sprite) = archive.decode_image(0, &active_pal) {
                let rgba = sprite.rgba.clone();
                let remappable = fmt & 0x01 != 0 || portrait_has_remap_pixels(&rgba);
                return Some((sprite.width as usize, sprite.height as usize, rgba, remappable));
            }
        }
        if let Some(sprite) = decode_fsh_sprites(&fsh, &active_pal).into_iter().next() {
            let remappable = fmt & 0x01 != 0 || portrait_has_remap_pixels(&sprite.rgba);
            return Some((sprite.width as usize, sprite.height as usize, sprite.rgba, remappable));
        }
        return None;
    }

    let decoded_owned;
    let pixels: &[u8] = if header == b"FPRT10" {
        decoded_owned = decompress_furcadia_lzw(&data[9..])?;
        &decoded_owned
    } else if header == b"FPRT20" {
        &data[9..]
    } else {
        return None;
    };

    if fmt == 3 || fmt == 4 || pixels.starts_with(b"BZh") {
        let fox_owned;
        let fox_bytes = if pixels.starts_with(b"BZh") {
            fox_owned = decompress_bzip2(pixels)?;
            fox_owned
        } else {
            pixels.to_vec()
        };
        if let Ok(mut archive) = FoxArchive::from_bytes(fox_bytes) {
            let active_pal = if fmt != 3 {
                palette.with_classic_remap_markers()
            } else {
                palette.clone()
            };
            if let Ok(sprite) = archive.decode_image(0, &active_pal) {
                let rgba = sprite.rgba.clone();
                let remappable = portrait_has_remap_pixels(&rgba);
                return Some((sprite.width as usize, sprite.height as usize, rgba, remappable));
            }
        }
        return None;
    }

    if fmt == 2 || pixels.len() >= w * h * 3 {
        if pixels.len() < w * h * 3 {
            return None;
        }
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            let src_y = h - 1 - y;
            for x in 0..w {
                let s = (src_y * w + x) * 3;
                let d = (y * w + x) * 4;
                rgba[d] = pixels[s + 2];
                rgba[d + 1] = pixels[s + 1];
                rgba[d + 2] = pixels[s];
                rgba[d + 3] = 255;
            }
        }
        let remappable = portrait_has_remap_pixels(&rgba);
        return Some((w, h, rgba, remappable));
    }

    None
}

fn main() -> eframe::Result<()> {
    let startup_cfg = ClientConfig::load_default();
    let icon = image::load_from_memory(include_bytes!("../../packaging/linux/furcadia-native.png"))
        .expect("application icon must be a valid PNG")
        .to_rgba8();
    let (icon_width, icon_height) = icon.dimensions();
    #[cfg(target_os = "linux")]
    let event_loop_builder: Option<eframe::EventLoopBuilderHook> = {
        let has_x11 = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());
        let user_forced_wayland = std::env::var("WINIT_UNIX_BACKEND")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false);
        if has_x11 && !user_forced_wayland {
            // winit 0.30 removed WINIT_UNIX_BACKEND and its Wayland backend ignores
            // set_visible(false), is_minimized(), and set_minimized(false). Use X11/XWayland
            // via EventLoopBuilderExtX11::with_x11 so minimize-to-tray, close-to-tray,
            // Hide to Tray, and click-tray-to-restore work natively on KDE/GNOME/Wayland.
            Some(Box::new(|builder| {
                use winit::platform::x11::EventLoopBuilderExtX11;
                builder.with_x11();
            }))
        } else {
            None
        }
    };
    #[cfg(not(target_os = "linux"))]
    let event_loop_builder: Option<eframe::EventLoopBuilderHook> = None;
    let options = eframe::NativeOptions {
        event_loop_builder,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([
                f32::from(startup_cfg.window_width),
                f32::from(startup_cfg.window_height),
            ])
            .with_maximized(startup_cfg.window_maximized)
            .with_title("Furcadia Native Client")
            // Wayland gets the window icon from the matching desktop entry,
            // since its backend ignores per-window icon data.
            .with_app_id("furcadia-native")
            .with_icon(egui::IconData {
                rgba: icon.into_raw(),
                width: icon_width,
                height: icon_height,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "Furcadia",
        options,
        Box::new(|cc| Ok(Box::new(FurcadiaApp::new(cc)))),
    )
}

fn ordered_character_tab_names(
    bg_names: &[&str],
    active_name: Option<&str>,
    active_pos: usize,
) -> Vec<String> {
    let mut names: Vec<String> = bg_names.iter().map(|s| (*s).to_owned()).collect();
    if let Some(active) = active_name {
        let insert_at = active_pos.min(names.len());
        names.insert(insert_at, active.to_owned());
    }
    names
}

fn format_character_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn format_close_all_button_label(names: &[String]) -> String {
    let joined = names.join(", ");
    if joined.chars().count() <= 40 {
        format!("Close All ({joined})")
    } else {
        format!("Close All ({} characters)", names.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_emote_quotes_handles_multiple_spoken_segments() {
        let input = r#"moves to find a seat and smiles. "Hey what's up?" the beast says as he smiles to everyone then licks his lips. "Why hello, ladies." he winks to the females across the way"#;
        let parts = split_emote_quotes(input);
        assert_eq!(
            parts,
            vec![
                ("moves to find a seat and smiles. ", false),
                ("\"Hey what's up?\"", true),
                (" the beast says as he smiles to everyone then licks his lips. ", false),
                ("\"Why hello, ladies.\"", true),
                (" he winks to the females across the way", false),
            ]
        );
    }

    #[test]
    fn rewrite_pounce_add_message_reformats_friend_request() {
        let line = parse_chat_line(
            "<font color='success'>Your friend request has been sent to <a href='name://kiralyrica'>Kira Lyrica</a>.</font>",
        );
        let rewritten = rewrite_pounce_add_message(line);
        assert_eq!(rewritten.to_plain_text(), "Kira Lyrica has been added to Pounce");
    }

    #[test]
    fn format_whisper_tab_line_does_not_repeat_pipe_name_in_emote() {
        let plain = r#"[ Alty Bunnie whispers, "Alty|Bunnie shivers and waves." to you. ]"#;
        let fallback = parse_chat_line(plain);
        let formatted = format_whisper_tab_line("Amala|Devi", "Alty Bunnie", plain, &fallback);
        assert_eq!(formatted.to_plain_text(), "Alty Bunnie shivers and waves.");
    }

    #[test]
    fn outgoing_whisper_echo_extracts_trimmed_target_and_message() {
        let plain = r#"[ You whisper "I wanted to say hello." to Alty|Bunnie . ]"#;
        assert_eq!(extract_whisper_peer(plain).as_deref(), Some("Alty Bunnie"));
        assert_eq!(
            extract_outgoing_whisper_echo(plain),
            Some(("Alty Bunnie".to_owned(), "I wanted to say hello.".to_owned()))
        );
    }

    #[test]
    fn portrait_directive_uses_pp_file_id_not_character_id() {
        assert_eq!(
            parse_portrait_directive("43091366 pp7821"),
            (Some(43_091_366), Some(7_821), None)
        );
        assert_eq!(
            parse_portrait_directive("43091366 pp7821 Bentley|Karthorn"),
            (
                Some(43_091_366),
                Some(7_821),
                Some("Bentley|Karthorn".to_owned())
            )
        );
    }

    #[test]
    fn custom_portrait_id_prefers_known_map_and_filters_slot_indices() {
        let mut known = HashMap::new();
        known.insert("amaladevi".to_owned(), 248_815);

        // Saved / ]&-resolved portrait ID takes precedence over slot or stale last_port.
        assert_eq!(
            resolve_custom_portrait_id(&known, "amaladevi", Some(3)),
            Some(248_815)
        );
        assert_eq!(
            resolve_custom_portrait_id(&known, "amaladevi", Some(12345)),
            Some(248_815)
        );
        // Slot indices (1..=16) in last_port are ignored when no pp<id> is known yet.
        assert_eq!(
            resolve_custom_portrait_id(&known, "aureusktharr", Some(5)),
            None
        );
        // Real pp<id> (> 16) in last_port is used when player_portrait_ids has no entry.
        assert_eq!(
            resolve_custom_portrait_id(&known, "aureusktharr", Some(257_927)),
            Some(257_927)
        );
        // Empty name keys never match.
        assert_eq!(resolve_custom_portrait_id(&known, "", Some(257_927)), None);
    }

    #[test]
    fn silent_tab_glook_text_suppression_only_hides_matching_look_and_desc() {
        let t0 = Instant::now();
        let mut pending = vec![SilentTabGlook {
            target_key: "alice".to_owned(),
            sent_at: t0,
            card_received: true,
            saw_you_see: false,
        }];

        // Unrelated speech before `(You see Alice.)` is not suppressed.
        assert!(!should_suppress_silent_tab_glook_text(&mut pending, "Bob: hello", t0));
        // Another player's look line is not suppressed.
        assert!(!should_suppress_silent_tab_glook_text(&mut pending, "(You see Bob.)", t0));
        // Matching `(You see Alice.)` and its immediate `> ` description line are suppressed.
        assert!(should_suppress_silent_tab_glook_text(&mut pending, "(You see Alice.)", t0));
        assert!(should_suppress_silent_tab_glook_text(&mut pending, "> A tall fox.", t0));
        // Entry is removed once both `card_received` and `> ` desc are consumed; a second `> ` line is shown.
        assert!(pending.is_empty());
        assert!(!should_suppress_silent_tab_glook_text(&mut pending, "> Normal speech.", t0));

        // Expired (>5s) entries are pruned and never suppress.
        let mut expired = vec![SilentTabGlook {
            target_key: "alice".to_owned(),
            sent_at: t0,
            card_received: false,
            saw_you_see: false,
        }];
        let t_late = t0 + Duration::from_secs(6);
        assert!(!should_suppress_silent_tab_glook_text(
            &mut expired,
            "(You see Alice.)",
            t_late
        ));
        assert!(expired.is_empty());
    }

    #[test]
    fn resolve_and_switch_tab_portraits() {
        let make_card = |name: &str, pid: Option<u32>| LookedPlayerCard {
            info: LookedPlayerInfo {
                name: name.to_owned(),
                appearance: AvatarAppearance::default(),
            },
            short_name: wire_player_name(name),
            portrait_id: pid,
            portrait_tex: None,
            badge_tex: None,
            resolved_for: None,
        };

        let mut active_chat_tab: Option<String> = None;
        let charlie = make_card("Charlie", Some(101));
        let mut looked_player = Some(charlie.clone());
        let mut main_looked_player = Some(charlie);
        let mut known_player_cards = HashMap::new();
        let player_portrait_ids = HashMap::new();
        let avatars = std::collections::BTreeMap::new();
        let mut whispers = vec![
            WhisperSession::new("Alice".to_owned()),
            WhisperSession::new("Bob".to_owned()),
        ];
        cache_looked_player_card_into(
            &make_card("Alice", Some(201)),
            &mut known_player_cards,
            &mut whispers,
        );
        cache_looked_player_card_into(
            &make_card("Bob", Some(202)),
            &mut known_player_cards,
            &mut whispers,
        );

        let switch_tab = |target: Option<&str>,
                          active_tab: &mut Option<String>,
                          looked: &mut Option<LookedPlayerCard>,
                          main_looked: &mut Option<LookedPlayerCard>,
                          whispers: &mut [WhisperSession],
                          known: &HashMap<String, LookedPlayerCard>| {
            let same = match (active_tab.as_deref(), target) {
                (None, None) => true,
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => false,
            };
            if same {
                if let Some(peer) = target {
                    let cur_key = looked.as_ref().map(|c| shortname_key(&c.short_name));
                    if cur_key.as_deref() != Some(shortname_key(peer).as_str()) {
                        sync_tab_portrait_state(
                            active_tab.as_deref(),
                            false,
                            looked,
                            main_looked,
                            whispers,
                            known,
                            &player_portrait_ids,
                            &avatars,
                        );
                    }
                }
                return;
            }
            if active_tab.is_none() {
                *main_looked = looked.clone();
            }
            *active_tab = target.map(str::to_owned);
            sync_tab_portrait_state(
                active_tab.as_deref(),
                false,
                looked,
                main_looked,
                whispers,
                known,
                &player_portrait_ids,
                &avatars,
            );
        };

        // Main -> Whisper Alice shows Alice's portrait.
        switch_tab(
            Some("Alice"),
            &mut active_chat_tab,
            &mut looked_player,
            &mut main_looked_player,
            &mut whispers,
            &known_player_cards,
        );
        assert_eq!(
            looked_player.as_ref().map(|c| c.short_name.as_str()),
            Some("Alice")
        );
        assert_eq!(looked_player.as_ref().and_then(|c| c.portrait_id), Some(201));

        // Whisper Alice -> Whisper Bob shows Bob's portrait.
        switch_tab(
            Some("Bob"),
            &mut active_chat_tab,
            &mut looked_player,
            &mut main_looked_player,
            &mut whispers,
            &known_player_cards,
        );
        assert_eq!(
            looked_player.as_ref().map(|c| c.short_name.as_str()),
            Some("Bob")
        );

        // Whisper Bob -> Main restores Charlie.
        switch_tab(
            None,
            &mut active_chat_tab,
            &mut looked_player,
            &mut main_looked_player,
            &mut whispers,
            &known_player_cards,
        );
        assert_eq!(
            looked_player.as_ref().map(|c| c.short_name.as_str()),
            Some("Charlie")
        );

        // Dismiss on Main clears Main's remembered card, then Whisper Alice -> Main stays None.
        looked_player = None;
        main_looked_player = None;
        switch_tab(
            Some("Alice"),
            &mut active_chat_tab,
            &mut looked_player,
            &mut main_looked_player,
            &mut whispers,
            &known_player_cards,
        );
        assert_eq!(
            looked_player.as_ref().map(|c| c.short_name.as_str()),
            Some("Alice")
        );
        switch_tab(
            None,
            &mut active_chat_tab,
            &mut looked_player,
            &mut main_looked_player,
            &mut whispers,
            &known_player_cards,
        );
        assert!(looked_player.is_none());

        // Explicit look for Dave while on Whisper Alice updates Main's remembered card without
        // overwriting Alice's stored whisper card; re-clicking Alice snaps back to Alice.
        switch_tab(
            Some("Alice"),
            &mut active_chat_tab,
            &mut looked_player,
            &mut main_looked_player,
            &mut whispers,
            &known_player_cards,
        );
        let mut pending = Vec::new();
        apply_looked_card_to_tabs(
            make_card("Dave", Some(303)),
            active_chat_tab.as_deref(),
            &mut looked_player,
            &mut main_looked_player,
            &mut whispers,
            &mut known_player_cards,
            &mut pending,
            Instant::now(),
        );
        assert_eq!(
            looked_player.as_ref().map(|c| c.short_name.as_str()),
            Some("Dave")
        );
        switch_tab(
            Some("Alice"),
            &mut active_chat_tab,
            &mut looked_player,
            &mut main_looked_player,
            &mut whispers,
            &known_player_cards,
        );
        assert_eq!(
            looked_player.as_ref().map(|c| c.short_name.as_str()),
            Some("Alice")
        );
        switch_tab(
            None,
            &mut active_chat_tab,
            &mut looked_player,
            &mut main_looked_player,
            &mut whispers,
            &known_player_cards,
        );
        assert_eq!(
            looked_player.as_ref().map(|c| c.short_name.as_str()),
            Some("Dave")
        );
    }

    #[test]
    fn cookie_counts_parse_server_reports() {
        assert_eq!(
            parse_cookie_counts("[@cookie] Cookie bank has currently collected: 7"),
            (Some(7), None)
        );
        assert_eq!(
            parse_cookie_counts("[@cookie] All-time Cookie total: 42"),
            (None, Some(42))
        );
        assert_eq!(parse_cookie_counts("hello world"), (None, None));
    }

    #[test]
    fn bundled_spellcheck_recognizes_furcadia_and_flags_typos() {
        let custom = std::collections::HashSet::new();
        let ok = run_bundled_spell_check("Furcadia handpaws nuzzled Alty's", &custom);
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].word, "Alty's");

        let typos = run_bundled_spell_check("teh furre", &custom);
        assert_eq!(typos.len(), 1);
        assert_eq!(typos[0].word, "teh");
        assert!(typos[0].suggestions.iter().any(|s| s == "the"));
    }

    #[test]
    fn main_map_ids_normalize_to_apollo_names() {
        assert_eq!(normalize_main_map_id("allegria023"), Some("pmallegria"));
        assert_eq!(normalize_main_map_id("pmimag"), Some("pmimaginarium"));
        assert_eq!(normalize_main_map_id("imagine"), Some("pmimaginarium"));
        assert_eq!(normalize_main_map_id("acropoli"), Some("pmacropolis"));
        assert_eq!(normalize_main_map_id("furn"), Some("pmfurn"));
    }

    #[test]
    fn update_version_comparison_and_json_parsing() {
        assert!(is_newer_version("1.0.2", "v1.0.3"));
        assert!(is_newer_version("1.0.2", "1.1.0"));
        assert!(!is_newer_version("1.0.2", "v1.0.2"));
        assert!(!is_newer_version("1.0.2", "1.0.1"));

        let sample = r#"{
            "tag_name": "v1.0.3",
            "html_url": "https://github.com/morganlowe/native-furcadia/releases/tag/v1.0.3",
            "assets": [
                {"browser_download_url": "https://github.com/morganlowe/native-furcadia/releases/download/v1.0.3/FurcadiaNative-Setup-1.0.3-x86_64.exe"},
                {"browser_download_url": "https://github.com/morganlowe/native-furcadia/releases/download/v1.0.3/FurcadiaNative-1.0.3-macos-arm64.dmg"},
                {"browser_download_url": "https://github.com/morganlowe/native-furcadia/releases/download/v1.0.3/furcadia-native-debug-1.0.3-1-x86_64.pkg.tar.zst"},
                {"browser_download_url": "https://github.com/morganlowe/native-furcadia/releases/download/v1.0.3/furcadia-native-1.0.3-1-x86_64.pkg.tar.zst"},
                {"browser_download_url": "https://github.com/morganlowe/native-furcadia/releases/download/v1.0.3/furcadia-native-1.0.3-1.x86_64.rpm"},
                {"browser_download_url": "https://github.com/morganlowe/native-furcadia/releases/download/v1.0.3/furcadia-native_1.0.3_amd64.deb"}
            ]
        }"#;
        let info = parse_github_release_json(sample).expect("should parse release json");
        assert_eq!(info.tag, "v1.0.3");
        assert_eq!(info.version, "1.0.3");
        assert_eq!(
            info.html_url,
            "https://github.com/morganlowe/native-furcadia/releases/tag/v1.0.3"
        );
        assert!(
            info.asset_url
                .as_deref()
                .is_some_and(|u| !u.contains("-debug-")),
            "expected matching non-debug release asset URL on host platform, got {:?}",
            info.asset_url
        );
        assert_eq!(
            update_download_filename("https://example.com/releases/FurcadiaNative-Setup-1.0.3-x86_64.exe?dl=1"),
            "FurcadiaNative-Setup-1.0.3-x86_64.exe"
        );
        assert_eq!(
            update_download_filename("https://example.com/furcadia-native_1.0.3_amd64.deb"),
            "furcadia-native_1.0.3_amd64.deb"
        );
    }

    #[test]
    fn select_linux_asset_url_matches_arch_rpm_and_deb() {
        let urls = vec![
            "https://example.com/furcadia-native-debug-2.2.0-1-x86_64.pkg.tar.zst".to_owned(),
            "https://example.com/furcadia-native-2.2.0-1-x86_64.pkg.tar.zst".to_owned(),
            "https://example.com/furcadia-native-2.2.0-1.x86_64.rpm".to_owned(),
            "https://example.com/furcadia-native_2.2.0_amd64.deb".to_owned(),
            "https://example.com/FurcadiaNative-2.2.0-macos-arm64.dmg".to_owned(),
        ];
        assert_eq!(
            select_linux_asset_url(&urls, true, false).as_deref(),
            Some("https://example.com/furcadia-native-2.2.0-1-x86_64.pkg.tar.zst")
        );
        assert_eq!(
            select_linux_asset_url(&urls, false, true).as_deref(),
            Some("https://example.com/furcadia-native-2.2.0-1.x86_64.rpm")
        );
        assert_eq!(
            select_linux_asset_url(&urls, false, false).as_deref(),
            Some("https://example.com/furcadia-native_2.2.0_amd64.deb")
        );
    }

    #[test]
    fn decode_furcadia_portrait_handles_fprt20_bottom_up_bgr() {
        // 2x1 bottom-up BGR: row 0 (bottom) = [B=1,G=2,R=3], [B=4,G=5,R=6]
        let mut data = b"FPRT20\x02\x02\x02".to_vec();
        // bottom row (y=1 in top-down)
        data.extend_from_slice(&[10, 20, 30, 40, 50, 60]);
        // top row (y=0 in top-down)
        data.extend_from_slice(&[100, 110, 120, 130, 140, 150]);
        let (w, h, rgba, remappable) = decode_furcadia_portrait(&data, &Palette::default())
            .expect("should decode FPRT20");
        assert_eq!((w, h), (2, 2));
        assert!(!remappable);
        // Top-left pixel should be RGB(120, 110, 100, 255)
        assert_eq!(&rgba[0..4], &[120, 110, 100, 255]);
        // Bottom-right pixel should be RGB(60, 50, 40, 255)
        assert_eq!(&rgba[12..16], &[60, 50, 40, 255]);
    }

    #[test]
    fn decode_furcadia_portrait_remaps_classic_fshx_palette_indices() {
        // Construct a minimal 10x10 FSHX portrait inside FPRT20 (fmt=4) using classic 8-bit
        // remap indices (Fur 200, Markings 136, Hair 128, Boots 16, Cape 32, Vest 72,
        // Bracers 88, Trousers 224, Eyes 50, Badge 11) plus static white (208).
        let mut fshx = Vec::new();
        fshx.extend_from_slice(b"FSHX");
        fshx.extend_from_slice(&1u32.to_le_bytes()); // version = 1
        fshx.extend_from_slice(&1u32.to_le_bytes()); // obj_count = 1
        fshx.extend_from_slice(&[0u8; 16]); // padding to 28-byte FSHX header
        // Object 0 (8-byte header): raw_flags=0, number=0, frame_count=1, anim_count=0
        fshx.extend_from_slice(&0u16.to_le_bytes());
        fshx.extend_from_slice(&0i16.to_le_bytes());
        fshx.extend_from_slice(&1u16.to_le_bytes());
        fshx.extend_from_slice(&0u16.to_le_bytes());
        // Frame 0 (18-byte header): pad(2)=1, w=10, h=10, ox=0, oy=0, ex=0, ey=0, a=100 (w*h)
        fshx.extend_from_slice(&1u16.to_le_bytes());
        fshx.extend_from_slice(&10u16.to_le_bytes());
        fshx.extend_from_slice(&10u16.to_le_bytes());
        fshx.extend_from_slice(&[0u8; 8]);
        fshx.extend_from_slice(&100u32.to_le_bytes());
        // 100 bottom-up indexed pixels: 80 remap pixels (>= 64 threshold) + 20 static white (208)
        let remap_sample = [200u8, 136, 128, 16, 32, 72, 88, 224, 50, 11];
        for row in 0..10 {
            for &idx in &remap_sample {
                if row < 8 {
                    fshx.push(idx);
                } else {
                    fshx.push(208); // static white
                }
            }
        }

        let mut data = b"FPRT20\x0a\x0a\x04".to_vec();
        data.extend_from_slice(&fshx);

        let pal = Palette::default();
        let (w, h, rgba, remappable) =
            decode_furcadia_portrait(&data, &pal).expect("should decode FSHX FPRT20");
        assert_eq!((w, h), (10, 10));
        assert!(remappable, "classic 8-bit FSHX portrait should be detected as remappable");

        // Top two rows in top-down RGBA correspond to bottom-up rows 8..10 (static white 208).
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);

        // Applying two different appearances should recolor the remapped rows while keeping static white.
        let app_a = furcadia_client::parse_color_code("w#############$#");
        let app_b = furcadia_client::parse_color_code("w))))))))))#$#");
        let recolored_a = RemapLuts::from_appearance(&app_a).apply_rgba(&rgba);
        let recolored_b = RemapLuts::from_appearance(&app_b).apply_rgba(&rgba);
        assert_eq!(&recolored_a[0..4], &[255, 255, 255, 255]);
        assert_eq!(&recolored_b[0..4], &[255, 255, 255, 255]);
        assert_ne!(
            &recolored_a[20 * 4..100 * 4],
            &recolored_b[20 * 4..100 * 4],
            "remapped pixels must change when avatar color slots change"
        );
    }

    #[test]
    fn rewrite_pounce_add_message_does_not_panic_on_multibyte_text() {
        // Byte 36 (prefix length) falls inside the 3-byte full-width 'Ｅ'.
        let line = parse_chat_line("(Your friend request has been sentＥＥＥ to someone");
        let out = rewrite_pounce_add_message(line.clone());
        assert_eq!(out.to_plain_text(), line.to_plain_text());
        let line2 = parse_chat_line("(ＬＩＶＥ ＥＶＥＮＴ ＮＯＷ ＯＰＥＮ ＩＮ ＴＨＥ ＤＲＥＡＭ");
        let _ = rewrite_pounce_add_message(line2);
    }

    #[test]
    fn decompress_furcadia_lzw_decodes_inverted_msb_stream() {
        // 9-bit MSB-first codes: CLEAR(0x100)=100000000, 'A'(0x41)=001000001, 'B'(0x42)=001000010, 0x102("AB")=100000010, EOF(0x101)=100000001
        // Bits: 10000000 00010000 01001000 01010000 00101000 00001_000 (padded)
        // Inverted (^ 0xff):
        let encoded = [0x80 ^ 0xff, 0x10 ^ 0xff, 0x48 ^ 0xff, 0x50 ^ 0xff, 0x28 ^ 0xff, 0x08 ^ 0xff];
        let dec = decompress_furcadia_lzw(&encoded).expect("should decompress LZW");
        assert_eq!(dec, b"ABAB");
    }

    #[test]
    fn decompress_furcadia_lzw_handles_13bit_clear_at_table_full() {
        // Build an inverted MSB-first LZW stream that fills the dictionary to 4095,
        // writes the table-full CLEAR code at 13 bits (matching Furcadia's FPRT10 encoder),
        // then resumes at 9 bits and writes "OK".
        let mut bitbuf: u64 = 0;
        let mut bitlen: u32 = 0;
        let mut bytes: Vec<u8> = Vec::new();
        let mut emit = |code: u16, nbits: u32| {
            bitbuf = (bitbuf << nbits) | u64::from(code);
            bitlen += nbits;
            while bitlen >= 8 {
                bitlen -= 8;
                let b = ((bitbuf >> bitlen) & 0xff) as u8;
                bytes.push(b ^ 0xff);
            }
        };

        let mut nbits: u32 = 9;
        let mut next_id: u16 = 0x102;
        emit(0x100, nbits); // CLEAR
        emit(0x41, nbits); // 'A' (initial code, does not add to table)
        // Emit literal bytes until next_id reaches 4095 (4095 - 258 = 3837 additions)
        let mut expected = vec![0x41u8];
        for i in 0..3837u32 {
            let sym = ((i % 251) as u8) as u16;
            emit(sym, nbits);
            expected.push(sym as u8);
            next_id += 1;
            if (next_id as u32) + 1 == (1u32 << nbits) {
                nbits += 1;
            }
        }
        assert_eq!(next_id, 4095);
        assert_eq!(nbits, 13);
        // Table-full CLEAR written at 13 bits, then reset to 9 bits
        emit(0x100, 13);
        nbits = 9;
        emit(b'O' as u16, nbits);
        emit(b'K' as u16, nbits);
        emit(0x101, nbits); // EOF
        expected.extend_from_slice(b"OK");
        if bitlen > 0 {
            let b = ((bitbuf << (8 - bitlen)) & 0xff) as u8;
            bytes.push(b ^ 0xff);
        }

        let dec = decompress_furcadia_lzw(&bytes).expect("should decompress 13-bit CLEAR stream");
        assert_eq!(dec, expected);
    }

    #[test]
    fn parse_dynamic_avatar_details_uses_species_minus_135_and_separates_hop_flag() {
        // Payload after `]M`: '%' + server_code ' ' + three 8-byte entries.
        let mut data = vec![b'%', b' '];
        // Entry 0: species_id = 164 (lo = 164, hi_and_flag = 1 -> hop=true)
        data.extend_from_slice(&[36, 35, 35 + 164, 35 + 1, 35, 35, 35, 35]);
        // Entry 1: species_id = 260 (220 * 1 + 40, hi_and_flag = 2 -> hop=false)
        data.extend_from_slice(&[36, 35, 35 + 40, 35 + 2, 35, 35, 35, 35]);
        // Entry 2: species_id = 10 (<= 135 -> skipped)
        data.extend_from_slice(&[36, 35, 35 + 10, 35, 35, 35, 35, 35]);

        let (srv, entries) = parse_dynamic_avatar_details(&data).expect("should parse ]M%");
        assert_eq!(srv, None);
        assert_eq!(entries, vec![(164, 29, true), (260, 125, false)]);
    }

    #[test]
    fn resolve_species_spec_handles_stock_local_morphs_and_dynamic_avatars() {
        let empty = HashMap::new();
        assert_eq!(resolve_species_spec_with_dynamic(0, &empty), (1, 0));
        assert_eq!(resolve_species_spec_with_dynamic(4, &empty), (4, 0));
        assert_eq!(resolve_species_spec_with_dynamic(97, &empty), (104, 1));
        assert_eq!(resolve_species_spec_with_dynamic(98, &empty), (106, 1));
        assert_eq!(resolve_species_spec_with_dynamic(99, &empty), (119, 1));
        assert_eq!(resolve_species_spec_with_dynamic(108, &empty), (108, 0));
        assert_eq!(resolve_species_spec_with_dynamic(2104, &empty), (104, 1));
        assert_eq!(resolve_species_spec_with_dynamic(1001, &empty), (1001, 0));
        assert_eq!(
            resolve_species_spec_with_dynamic(136, &empty),
            (DYNAMIC_AVATAR_CACHE_BASE + 1, 0)
        );
        assert_eq!(
            resolve_species_spec_with_dynamic(164, &empty),
            (DYNAMIC_AVATAR_CACHE_BASE + 29, 0)
        );

        let with_hop = HashMap::from([(164u16, (29usize, true))]);
        assert_eq!(
            resolve_species_spec_with_dynamic(164, &with_hop),
            (DYNAMIC_AVATAR_CACHE_BASE + 29, 0)
        );
        assert_eq!(
            resolve_species_spec_with_dynamic(2164, &with_hop),
            (DYNAMIC_AVATAR_CACHE_BASE + 29, 1)
        );
    }

    #[test]
    fn format_bytes_formats_human_readable_sizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MB");
    }

    #[test]
    fn split_chat_segments_highlights_brackets_in_chat_whispers_and_emotes() {
        assert_eq!(
            split_chat_segments("Hello [contained in these brackets] world", false, true),
            vec![
                ("Hello ", ChatSegmentKind::Normal),
                ("[contained in these brackets]", ChatSegmentKind::Bracketed),
                (" world", ChatSegmentKind::Normal),
            ]
        );
        assert_eq!(
            split_chat_segments("[ooc] hi [brb]", false, true),
            vec![
                ("[ooc]", ChatSegmentKind::Bracketed),
                (" hi ", ChatSegmentKind::Normal),
                ("[brb]", ChatSegmentKind::Bracketed),
            ]
        );
        assert_eq!(
            split_chat_segments("hey [outer [inner] end] ok", false, true),
            vec![
                ("hey ", ChatSegmentKind::Normal),
                ("[outer [inner] end]", ChatSegmentKind::Bracketed),
                (" ok", ChatSegmentKind::Normal),
            ]
        );
        assert_eq!(
            split_chat_segments("[oops [ok]", false, true),
            vec![
                ("[oops ", ChatSegmentKind::Normal),
                ("[ok]", ChatSegmentKind::Bracketed),
            ]
        );
        assert_eq!(
            split_chat_segments("empty [] stays plain", false, true),
            vec![("empty [] stays plain", ChatSegmentKind::Normal)]
        );
        assert_eq!(
            split_chat_segments("waves \"hi [ooc] there\" [brb]", true, true),
            vec![
                ("waves ", ChatSegmentKind::Normal),
                ("\"hi ", ChatSegmentKind::EmoteQuote),
                ("[ooc]", ChatSegmentKind::Bracketed),
                (" there\"", ChatSegmentKind::EmoteQuote),
                (" ", ChatSegmentKind::Normal),
                ("[brb]", ChatSegmentKind::Bracketed),
            ]
        );
        assert_eq!(
            split_chat_segments("waves [ooc: \"test\"]", true, true),
            vec![
                ("waves ", ChatSegmentKind::Normal),
                ("[ooc: \"test\"]", ChatSegmentKind::Bracketed),
            ]
        );
    }

    #[test]
    fn should_highlight_brackets_only_targets_chat_and_whisper_styles() {
        let base = furcadia_client::ChatStyle::default();
        assert!(should_highlight_brackets(&base));

        for name in ["speech", "myspeech", "emote", "whisper", "shout"] {
            let st = furcadia_client::ChatStyle {
                color: Some(name.to_owned()),
                ..base.clone()
            };
            assert!(should_highlight_brackets(&st), "expected {name} to highlight brackets");
        }

        for name in ["success", "warning", "error", "emit", "system"] {
            let st = furcadia_client::ChatStyle {
                color: Some(name.to_owned()),
                ..base.clone()
            };
            assert!(!should_highlight_brackets(&st), "did not expect {name} to highlight brackets");
        }

        let linked = furcadia_client::ChatStyle {
            color: Some("speech".to_owned()),
            link: Some("https://example.com".to_owned()),
            ..base
        };
        assert!(!should_highlight_brackets(&linked));
    }

    #[test]
    fn trade_and_gift_commands_route_as_speech_prefixed_commands() {
        let assert_say = |raw: &str, peer: Option<&str>, expected_text: &str| {
            let (cmd, open_tab) = classify_chat_input(raw, peer, false);
            assert!(
                open_tab.is_none(),
                "unexpected whisper tab open for {raw:?}: {open_tab:?}"
            );
            match &cmd {
                ClientCommand::Say(text) => {
                    assert_eq!(text, expected_text, "input {raw:?}");
                    assert_eq!(
                        cmd.encode().unwrap(),
                        format!("\"{expected_text}\n"),
                        "wire encode for {raw:?}"
                    );
                }
                other => panic!(
                    "expected ClientCommand::Say for {raw:?}, got {:?}",
                    other.encode()
                ),
            }
        };

        // Bare trade/gift/give-* commands in Main Chat.
        for cmd_text in [
            "trade-accept",
            "trade-start Bentlay|Karthorn",
            "trade-add portrait",
            "trade-reason test",
            "trade-status",
            "gift-portrait Bentlay|Karthorn hi",
            "give-rose Bentlay|Karthorn",
        ] {
            assert_say(cmd_text, None, cmd_text);
        }

        // Bare trade/gift/give-* commands in a Whisper tab must still send as speech, not whisper.
        for cmd_text in [
            "trade-accept",
            "trade-start Bentlay|Karthorn",
            "gift-portrait Bentlay|Karthorn hi",
            "give-rose Bentlay|Karthorn",
        ] {
            assert_say(cmd_text, Some("Bentlay Karthorn"), cmd_text);
        }

        // Plain English starting with "give" or normal text in a Whisper tab must whisper.
        for msg in ["give me a minute", "hello"] {
            let (cmd, open_tab) = classify_chat_input(msg, Some("Bentlay Karthorn"), false);
            assert!(open_tab.is_none());
            match cmd {
                ClientCommand::Whisper { target, message } => {
                    assert_eq!(target, "Bentlay|Karthorn");
                    assert_eq!(message, msg);
                }
                other => panic!(
                    "expected ClientCommand::Whisper for {msg:?}, got {:?}",
                    other.encode()
                ),
            }
        }

        // Slash-prefixed trade/gift/give commands strip '/' and route as speech.
        for (input, expected) in [
            ("/trade-accept", "trade-accept"),
            ("/trade-add portrait", "trade-add portrait"),
            ("/trade-start Bentlay|Karthorn", "trade-start Bentlay|Karthorn"),
            ("/gift-portrait Bentlay|Karthorn hi", "gift-portrait Bentlay|Karthorn hi"),
            ("/give Bentlay|Karthorn", "give Bentlay|Karthorn"),
        ] {
            assert_say(input, None, expected);
            assert_say(input, Some("Bentlay Karthorn"), expected);
        }

        // Backtick-prefixed speech commands vs raw server commands.
        for (input, expected) in [
            ("`trade-accept", "trade-accept"),
            ("`gift-portrait Bentlay|Karthorn", "gift-portrait Bentlay|Karthorn"),
        ] {
            assert_say(input, None, expected);
        }
        for (input, expected) in [("`info", "info"), ("`who", "who")] {
            let (cmd, open_tab) = classify_chat_input(input, None, false);
            assert!(open_tab.is_none());
            match cmd {
                ClientCommand::Raw(text) => assert_eq!(text, expected, "input {input:?}"),
                other => panic!(
                    "expected ClientCommand::Raw for {input:?}, got {:?}",
                    other.encode()
                ),
            }
        }

        // Escape hatch /raw sends verbatim ClientCommand::Raw.
        let (raw_cmd, open_tab) = classify_chat_input("/raw trade-accept", None, false);
        assert!(open_tab.is_none());
        match raw_cmd {
            ClientCommand::Raw(text) => assert_eq!(text, "trade-accept"),
            other => panic!("expected ClientCommand::Raw, got {:?}", other.encode()),
        }

        // command:// links and helper menus format wire string appropriately.
        assert_eq!(format_raw_cmd_wire("trade-accept"), "\"trade-accept");
        assert_eq!(format_raw_cmd_wire("gift-portrait Foo"), "\"gift-portrait Foo");
        assert_eq!(format_raw_cmd_wire("info"), "info");
    }

    #[test]
    fn trade_menu_command_building_and_wire_encoding() {
        assert_eq!(
            build_trade_command("trade-add", " portrait ").as_deref(),
            Some("trade-add portrait")
        );
        assert_eq!(
            build_trade_command("trade-add", " GD 50 ").as_deref(),
            Some("trade-add GD 50")
        );
        assert_eq!(
            build_trade_command("trade-reason", "art\ncommission\r\n").as_deref(),
            Some("trade-reason art commission")
        );
        assert_eq!(build_trade_command("trade-remove", "   \n\t "), None);

        for cmd in [
            "trade-start Kari#SO",
            "trade-start Bentlay|Karthorn",
            "trade-status",
            "trade-add portrait",
            "trade-add GD 50",
            "trade-remove portrait",
            "trade-reason commission",
            "trade-accept",
            "trade-reject",
            "trade-end",
        ] {
            assert_eq!(format_raw_cmd_wire(cmd), format!("\"{cmd}"));
        }

        for (group_name, items) in COMMON_TRADE_ITEM_GROUPS {
            assert!(!group_name.trim().is_empty());
            assert!(!items.is_empty());
            for (label, keyword) in *items {
                assert!(!label.trim().is_empty());
                assert!(!keyword.trim().is_empty());
                let expected_cmd = format!("trade-add {keyword}");
                assert_eq!(
                    build_trade_command("trade-add", keyword).as_deref(),
                    Some(expected_cmd.as_str()),
                    "unexpected build_trade_command for {label}"
                );
                assert_eq!(
                    format_raw_cmd_wire(&expected_cmd),
                    format!("\"{expected_cmd}"),
                    "unexpected wire command for {label}"
                );
            }
        }
    }

    #[test]
    fn ui_colors_map_chat_color_and_config_roundtrip() {
        let defs = UiColors::default();
        assert_eq!(defs.bracket_text, Color32::from_rgb(0, 92, 128));
        assert_eq!(defs.tab_active_bg, Color32::from_rgb(130, 200, 255));
        assert_eq!(defs.tab_unread_bg, Color32::from_rgb(250, 220, 50));
        assert!(!defs.custom_ui_bg);

        let mut custom = UiColors {
            chat_text: Color32::from_rgb(10, 20, 30),
            my_speech: Color32::from_rgb(11, 21, 31),
            bracket_text: Color32::from_rgb(12, 22, 32),
            emote_text: Color32::from_rgb(13, 23, 33),
            whisper_text: Color32::from_rgb(14, 24, 34),
            emit_text: Color32::from_rgb(15, 25, 35),
            shout_text: Color32::from_rgb(16, 26, 36),
            roll_text: Color32::from_rgb(17, 27, 37),
            success_text: Color32::from_rgb(18, 28, 38),
            warning_text: Color32::from_rgb(19, 29, 39),
            error_text: Color32::from_rgb(20, 30, 40),
            input_text: Color32::from_rgb(21, 31, 41),
            tab_active_bg: Color32::from_rgb(30, 40, 50),
            tab_active_text: Color32::from_rgb(31, 41, 51),
            tab_unread_bg: Color32::from_rgb(32, 42, 52),
            tab_unread_text: Color32::from_rgb(33, 43, 53),
            tab_inactive_bg: Color32::from_rgb(34, 44, 54),
            tab_inactive_text: Color32::from_rgb(35, 45, 55),
            custom_ui_bg: true,
            chat_bg: Color32::from_rgb(40, 50, 60),
            panel_bg: Color32::from_rgb(41, 51, 61),
            accent: Color32::from_rgb(42, 52, 62),
        };

        assert_eq!(custom.map_chat_color("speech"), custom.chat_text);
        assert_eq!(custom.map_chat_color("myspeech"), custom.my_speech);
        assert_eq!(custom.map_chat_color("emote"), custom.emote_text);
        assert_eq!(custom.map_chat_color("whisper"), custom.whisper_text);
        assert_eq!(custom.map_chat_color("emit"), custom.emit_text);
        assert_eq!(custom.map_chat_color("shout"), custom.shout_text);
        assert_eq!(custom.map_chat_color("roll"), custom.roll_text);
        assert_eq!(custom.map_chat_color("success"), custom.success_text);
        assert_eq!(custom.map_chat_color("warning"), custom.warning_text);
        assert_eq!(custom.map_chat_color("error"), custom.error_text);
        assert_eq!(custom.map_chat_color("unknown_tag"), custom.chat_text);
        assert_eq!(custom.map_chat_color("#112233"), Color32::from_rgb(0x11, 0x22, 0x33));
        assert_eq!(custom.map_chat_color("#abc"), Color32::from_rgb(0xaa, 0xbb, 0xcc));

        let mut cfg = furcadia_client::config::ClientConfig::default();
        custom.write_to_config(&mut cfg);
        let parsed = furcadia_client::config::ClientConfig::parse(&cfg.serialize());
        let roundtripped = UiColors::from_config(&parsed);
        assert_eq!(roundtripped, custom);

        custom.reset_chat_colors();
        assert_eq!(custom.chat_text, defs.chat_text);
        assert_eq!(custom.bracket_text, defs.bracket_text);
        assert_eq!(custom.tab_active_bg, Color32::from_rgb(30, 40, 50));
        assert!(custom.custom_ui_bg);

        custom.reset_tab_colors();
        assert_eq!(custom.tab_active_bg, defs.tab_active_bg);
        assert_eq!(custom.tab_unread_bg, defs.tab_unread_bg);
        assert!(custom.custom_ui_bg);

        custom.reset_window_colors();
        assert_eq!(custom, defs);
    }

    #[test]
    fn posture_commands_and_wire_pose_mapping() {
        assert_eq!(posture_wire_command(0), "stand");
        assert_eq!(posture_wire_command(1), "sit");
        assert_eq!(posture_wire_command(2), "liedown");
        assert_eq!(format_raw_cmd_wire(posture_wire_command(2)), "liedown");

        assert_eq!(posture_from_wire_pose(2), 0);
        assert_eq!(posture_from_wire_pose(4), 0);
        assert_eq!(posture_from_wire_pose(3), 1);
        assert_eq!(posture_from_wire_pose(6), 2);
    }

    #[test]
    fn mention_matching_handles_nicknames_word_boundaries_and_self_lines() {
        assert_eq!(
            find_chat_mention("Myuel hugs Aur!", "Aureus K'Tharr", "Aur, Bear"),
            Some("Aur")
        );
        assert_eq!(
            find_chat_mention("Myuel: Hey Aureus K'Tharr!", "Aureus K'Tharr", "Aur"),
            Some("Aureus K'Tharr")
        );
        // Word boundary prevents partial-word false positives.
        assert_eq!(
            find_chat_mention("Myuel: nice bear-taur aura!", "Aureus K'Tharr", "Aur"),
            None
        );
        // Self speech/emotes and system/Who lines are ignored.
        assert_eq!(
            find_chat_mention("Aureus K'Tharr cheers! Aur is here!", "Aureus K'Tharr", "Aur"),
            None
        );
        assert_eq!(
            find_chat_mention("Aureus K'Tharr: hi Aur", "Aureus K'Tharr", "Aur"),
            None
        );
        assert_eq!(
            find_chat_mention("Current players: Myuel, Aureus K'Tharr.", "Aureus K'Tharr", "Aur"),
            None
        );
        assert_eq!(
            parse_mention_nicknames(" Aur, Bear ; Amala \n"),
            vec!["Aur", "Bear", "Amala"]
        );
    }

    #[test]
    fn parse_http_content_length_handles_redirect_headers_and_case_insensitivity() {
        let headers = "HTTP/1.1 302 Found\r\nContent-Length: 0\r\nLocation: http://apollo.furcadia.com/dreams/pmallegria.zip\r\n\r\nHTTP/1.1 200 OK\r\ncontent-length: 20786286\r\nContent-Type: application/octet-stream\r\n\r\n";
        assert_eq!(parse_http_content_length(headers), Some(20_786_286));
        assert_eq!(parse_http_content_length("HTTP/1.1 200 OK\r\n\r\n"), None);
    }

    #[test]
    fn stock_patch_slot_maps_subdirs_filenames_and_validity() {
        let cases = [
            (StockPatchSlot::ModernFloors, "modern", "floors.fox", false, true),
            (StockPatchSlot::LegacyFloors, "legacy", "floors.fox", true, true),
            (StockPatchSlot::LegacyFloors2, "legacy", "floors2.fox", true, true),
            (StockPatchSlot::ModernFloore, "modern", "floore.fox", false, true),
            (StockPatchSlot::LegacyFloore, "legacy", "floore.fox", true, true),
            (StockPatchSlot::ModernItems, "modern", "items.fox", false, true),
            (StockPatchSlot::LegacyItem, "legacy", "item.fox", true, true),
            (StockPatchSlot::LegacyItem2, "legacy", "item2.fox", true, true),
            (StockPatchSlot::LegacyItem3, "legacy", "item3.fox", true, true),
            (StockPatchSlot::LegacyItem4, "legacy", "item4.fox", true, true),
            (StockPatchSlot::ModernIteme, "modern", "iteme.fox", false, true),
            (StockPatchSlot::LegacyIteme, "legacy", "iteme.fox", true, true),
            (StockPatchSlot::ModernPortals, "modern", "portals.fox", false, true),
            (StockPatchSlot::ModernPortalse, "modern", "portalse.fox", false, true),
            (StockPatchSlot::ModernEffects, "modern", "effects.fox", false, true),
            (StockPatchSlot::LegacyEffects, "legacy", "effects.fox", true, true),
            (StockPatchSlot::ModernEffectse, "modern", "effectse.fox", false, true),
            (StockPatchSlot::LegacyEffectse, "legacy", "effectse.fox", true, true),
            (StockPatchSlot::ModernJunction, "modern", "junction.fox", false, true),
            (StockPatchSlot::LegacyJunction, "legacy", "junction.fox", true, true),
            (StockPatchSlot::ModernWall(0), "modern", "wall.fox", false, true),
            (StockPatchSlot::LegacyWall(0), "legacy", "wall.fox", true, true),
            (StockPatchSlot::ModernWall(3), "modern", "wall_3.fox", false, true),
            (StockPatchSlot::LegacyWall(7), "legacy", "wall_7.fox", true, true),
            (StockPatchSlot::ModernWall(1), "modern", "wall_1.fox", false, false),
            (StockPatchSlot::LegacyWall(2), "legacy", "wall_2.fox", true, false),
            (StockPatchSlot::ModernWall(8), "modern", "wall_8.fox", false, false),
        ];
        for (slot, expected_subdir, expected_file, expected_legacy, expected_valid) in cases {
            assert_eq!(slot.subdir(), expected_subdir, "subdir for {slot:?}");
            assert_eq!(slot.filename(), expected_file, "filename for {slot:?}");
            assert_eq!(slot.is_legacy(), expected_legacy, "is_legacy for {slot:?}");
            assert_eq!(slot.is_valid(), expected_valid, "is_valid for {slot:?}");
        }
    }

    #[test]
    fn fox_sprite_cache_base_setters_and_clear_preserve_dream_archive() {
        fn synth_fsh(w: u8, h: u8) -> FoxArchive {
            let pix_len = (w as usize) * (h as usize);
            let chunk_size = (4 + pix_len) as u16;
            let mut raw = Vec::new();
            raw.extend_from_slice(&1u16.to_le_bytes());
            raw.extend_from_slice(&chunk_size.to_le_bytes());
            raw.push(w);
            raw.push(h);
            raw.push(0);
            raw.push(0);
            raw.extend(std::iter::repeat_n(1u8, pix_len));
            FoxArchive::from_bytes(raw).expect("synthetic fsh should parse")
        }

        let mut cache = FoxSpriteCache {
            default_archive: None,
            legacy_archive: None,
            dream_archive: Some(synth_fsh(2, 2)),
            shape_textures: HashMap::from([((0, 0, true), None)]),
            image_textures: HashMap::new(),
            avatar_textures: HashMap::new(),
            prefix: "test".to_owned(),
        };

        cache.set_legacy_archive(synth_fsh(3, 3));
        assert!(cache.shape_textures.is_empty());
        assert_eq!(
            cache.dream_archive.as_ref().map(|a| (a.images[0].width, a.images[0].height)),
            Some((2, 2))
        );
        assert_eq!(
            cache.legacy_archive.as_ref().map(|a| (a.images[0].width, a.images[0].height)),
            Some((3, 3))
        );

        cache.shape_textures.insert((1, 0, false), None);
        cache.set_default_archive(synth_fsh(4, 4));
        assert!(cache.shape_textures.is_empty());
        assert!(cache.dream_archive.is_some());
        assert!(cache.legacy_archive.is_some());
        assert_eq!(
            cache.default_archive.as_ref().map(|a| (a.images[0].width, a.images[0].height)),
            Some((4, 4))
        );

        cache.shape_textures.insert((2, 0, false), None);
        cache.clear_base_archives();
        assert!(cache.shape_textures.is_empty());
        assert!(cache.default_archive.is_none());
        assert!(cache.legacy_archive.is_none());
        assert!(cache.dream_archive.is_some());
    }

    #[test]
    fn ordered_character_tab_names_places_active_at_tab_pos() {
        let bg = ["Bree", "Cinder"];
        assert_eq!(
            ordered_character_tab_names(&bg, Some("Aria"), 0),
            vec!["Aria", "Bree", "Cinder"]
        );
        assert_eq!(
            ordered_character_tab_names(&bg, Some("Aria"), 1),
            vec!["Bree", "Aria", "Cinder"]
        );
        assert_eq!(
            ordered_character_tab_names(&bg, Some("Aria"), 2),
            vec!["Bree", "Cinder", "Aria"]
        );
        assert_eq!(
            ordered_character_tab_names(&bg, Some("Aria"), 99),
            vec!["Bree", "Cinder", "Aria"]
        );
        assert_eq!(
            ordered_character_tab_names(&bg, None, 1),
            vec!["Bree", "Cinder"]
        );
    }

    #[test]
    fn format_character_names_handles_counts() {
        assert_eq!(format_character_names(&[]), "");
        assert_eq!(format_character_names(&["Aria".to_owned()]), "Aria");
        assert_eq!(
            format_character_names(&["Aria".to_owned(), "Bree".to_owned()]),
            "Aria and Bree"
        );
        assert_eq!(
            format_character_names(&[
                "Aria".to_owned(),
                "Bree".to_owned(),
                "Cinder".to_owned()
            ]),
            "Aria, Bree, and Cinder"
        );
    }

    #[test]
    fn format_close_all_button_label_truncates_long_lists() {
        let short = vec!["Aria".to_owned(), "Bree".to_owned()];
        assert_eq!(format_close_all_button_label(&short), "Close All (Aria, Bree)");

        let long = vec![
            "VeryLongCharacterOne".to_owned(),
            "VeryLongCharacterTwo".to_owned(),
            "VeryLongCharacterThree".to_owned(),
        ];
        assert_eq!(
            format_close_all_button_label(&long),
            "Close All (3 characters)"
        );
    }

    fn synth_sfnt(include_fvar: bool) -> Vec<u8> {
        let mut tables: Vec<([u8; 4], Vec<u8>)> = vec![
            (*b"CFF ", vec![0u8; 16]),
            (*b"cmap", vec![0u8; 16]),
            (
                *b"head",
                {
                    let mut head = vec![0u8; 54];
                    head[18..20].copy_from_slice(&1000u16.to_be_bytes());
                    head
                },
            ),
            (*b"hhea", vec![0u8; 36]),
            (*b"maxp", vec![0u8; 6]),
        ];
        if include_fvar {
            tables.push((*b"fvar", vec![0u8; 16]));
        }
        tables.sort_by_key(|(tag, _)| *tag);

        let num_tables = tables.len() as u16;
        let header_len = 12 + tables.len() * 16;
        let mut out = Vec::with_capacity(header_len + 256);
        out.extend_from_slice(b"OTTO");
        out.extend_from_slice(&num_tables.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());

        let mut offset = header_len as u32;
        for (tag, payload) in &tables {
            out.extend_from_slice(tag);
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&offset.to_be_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            offset += payload.len() as u32;
        }
        for (_, payload) in &tables {
            out.extend_from_slice(payload);
        }
        out
    }

    #[test]
    fn static_sfnt_validator_rejects_invalid_and_variable_fonts() {
        assert!(!is_valid_static_sfnt(&[]));
        assert!(!is_valid_static_sfnt(b"not a font file"));
        assert!(!is_valid_static_sfnt(b"ttcf\x00\x01\x00\x00\x00\x00\x00\x02"));
        assert!(is_valid_static_sfnt(&synth_sfnt(false)));
        assert!(!is_valid_static_sfnt(&synth_sfnt(true)));
    }

    #[test]
    fn build_font_definitions_preserves_fallbacks_and_loads_in_egui() {
        let (regular, bold) = discover_system_ui_fonts();
        let (defs, has_bold) = build_font_definitions(regular, bold, None);
        let prop = defs
            .families
            .get(&egui::FontFamily::Proportional)
            .expect("Proportional family must exist");
        for fallback in ["Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"] {
            assert!(prop.iter().any(|name| name == fallback), "missing {fallback}");
        }
        if has_bold {
            let bold_chain = defs
                .families
                .get(&egui::FontFamily::Name(FONT_FAMILY_UI_BOLD.into()))
                .expect("ui_bold family must exist");
            for fallback in ["Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"] {
                assert!(
                    bold_chain.iter().any(|name| name == fallback),
                    "bold missing {fallback}"
                );
            }
        }

        let (fixed_defs, _) = build_font_definitions(None, None, Some(synth_sfnt(false)));
        let fixed_chain = fixed_defs
            .families
            .get(&egui::FontFamily::Name("fixedsys".into()))
            .expect("fixedsys family must exist");
        for fallback in ["Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"] {
            assert!(
                fixed_chain.iter().any(|name| name == fallback),
                "fixedsys missing {fallback}"
            );
        }

        let ctx = egui::Context::default();
        ctx.set_fonts(defs);
        ctx.begin_pass(egui::RawInput::default());
        let galley = ctx.fonts(|f| {
            f.layout_no_wrap(
                "Main Chat Harry Hardacre ● ─".to_owned(),
                FontId::proportional(13.0),
                Color32::WHITE,
            )
        });
        assert!(galley.size().x > 0.0 && galley.size().y > 0.0);
        if has_bold {
            let bold_galley = ctx.fonts(|f| {
                f.layout_no_wrap(
                    "Main Chat Harry Hardacre".to_owned(),
                    FontId::new(13.0, egui::FontFamily::Name(FONT_FAMILY_UI_BOLD.into())),
                    Color32::WHITE,
                )
            });
            assert!(bold_galley.size().x > 0.0 && bold_galley.size().y > 0.0);
        }
        let _ = ctx.end_pass();
    }

    #[test]
    fn apply_font_settings_uses_integer_text_style_sizes() {
        let ctx = egui::Context::default();
        for sz in 10u16..=22u16 {
            configure_text_styles(
                &ctx,
                f32::from(sz),
                egui::FontFamily::Proportional,
                egui::FontFamily::Name(FONT_FAMILY_UI_BOLD.into()),
            );
            let style = ctx.style();
            let body = &style.text_styles[&egui::TextStyle::Body];
            let button = &style.text_styles[&egui::TextStyle::Button];
            let small = &style.text_styles[&egui::TextStyle::Small];
            let heading = &style.text_styles[&egui::TextStyle::Heading];
            let mono = &style.text_styles[&egui::TextStyle::Monospace];
            assert_eq!(button.size, body.size);
            for s in [body.size, button.size, small.size, heading.size, mono.size] {
                assert_eq!(s.fract(), 0.0, "non-integer font size {s} for base {sz}");
            }
        }
    }
}
