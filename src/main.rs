mod half_block;
mod pet;
mod screen;
mod sixel;

use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::fmt::Write as FmtWrite;
use std::io::{Read, Stdout, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pet::{Anim, Mood, Pet};
use screen::{dump_to_string, Screen};

const DEBUG_DUMP_KEY: u8 = 0x1c;
const PET_KEY: u8 = 0x10; // Ctrl+P
const FEED_KEY: u8 = 0x06; // Ctrl+F
const HELP_KEY: u8 = 0x19; // Ctrl+Y
const TICK_MS: u64 = 100;

const SHEET_GRAY: &[u8] = include_bytes!("../assets/sprites/cat_gray.png");
const SHEET_GINGER: &[u8] = include_bytes!("../assets/sprites/cat_ginger.png");
const SHEET_WHITE: &[u8] = include_bytes!("../assets/sprites/cat_white.png");

#[derive(Clone, Copy)]
enum Renderer {
    Sixel,
    Kitty,
    HalfBlock,
}

#[derive(Clone)]
struct AppConfig {
    cat: Option<String>,
    renderer: Option<String>,
    no_relaunch: bool,
    debug_overlay: bool,
    pets: usize,
    event_mode: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            cat: None,
            renderer: None,
            no_relaunch: false,
            debug_overlay: false,
            pets: 1,
            event_mode: false,
        }
    }
}

#[derive(Default)]
struct InteractionState {
    pet_presses: u64,
    feed_presses: u64,
    help_visible: bool,
}

// Detect "we were double-clicked from Explorer" by checking how many
// processes are attached to the current console. A normal double-click
// spawns a fresh ConHost window with just our process attached. When
// scamp is run from an existing shell, both the shell and scamp are
// attached, so the count is >= 2.
#[cfg(windows)]
fn try_relaunch_in_wt() {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Console::GetConsoleProcessList;

    // Already inside Windows Terminal — nothing to do.
    if std::env::var("WT_SESSION").is_ok() {
        return;
    }
    // Explicit opt-out for users who want scamp to stay in their current terminal.
    if std::env::var("SCAMP_NO_RELAUNCH").is_ok() {
        return;
    }

    let console_processes = unsafe {
        let mut buf = [0u32; 4];
        GetConsoleProcessList(buf.as_mut_ptr(), buf.len() as u32)
    };
    // 0 means "no console" (e.g., piped). 1 means we own the console (double-click).
    // 2 or more means we inherited a console from a parent shell.
    if console_processes != 1 {
        return;
    }

    let wt_path = match which::which("wt.exe") {
        Ok(p) => p,
        Err(_) => return,
    };
    let our_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };

    // DETACHED_PROCESS so wt does not try to inherit our console handles.
    const DETACHED_PROCESS: u32 = 0x00000008;
    let spawned = std::process::Command::new(wt_path)
        .arg(our_path)
        .creation_flags(DETACHED_PROCESS)
        .spawn();
    if spawned.is_ok() {
        std::process::exit(0);
    }
}

fn detect_renderer() -> Renderer {
    if let Ok(forced) = std::env::var("SCAMP_RENDERER") {
        match forced.to_lowercase().as_str() {
            "sixel" => return Renderer::Sixel,
            "kitty" => return Renderer::Kitty,
            "halfblock" | "half_block" | "half-block" | "fallback" => return Renderer::HalfBlock,
            _ => {}
        }
    }
    // Heuristic: env vars set by terminals known to support sixel.
    if std::env::var("WT_SESSION").is_ok() {
        return Renderer::Sixel;
    }
    // Note: TERM_PROGRAM=vscode is intentionally NOT trusted. VS Code's
    // terminal (and forks like Cursor, Antigravity) only render sixel when
    // `terminal.integrated.enableImages` is enabled, which is off by default
    // in many setups. Users can still opt in via SCAMP_RENDERER=sixel.
    if let Ok(tp) = std::env::var("TERM_PROGRAM") {
        if matches!(
            tp.as_str(),
            "WezTerm" | "iTerm.app" | "ghostty" | "Konsole" | "mlterm"
        ) {
            return Renderer::Sixel;
        }
    }
    if let Ok(t) = std::env::var("TERM") {
        if t.contains("kitty")
            || t.contains("foot")
            || t.contains("contour")
            || t.contains("ghostty")
            || t.contains("mlterm")
        {
            return Renderer::Sixel;
        }
    }
    Renderer::HalfBlock
}

fn parse_bool(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".config").join("scamp").join("config.toml"))
}

fn load_config_file() -> AppConfig {
    let mut cfg = AppConfig::default();
    let Some(path) = config_path() else {
        return cfg;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return cfg;
    };
    for line in raw.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let value = v.trim().trim_matches('"').trim_matches('\'');
        match key {
            "cat" => cfg.cat = Some(value.to_string()),
            "renderer" => cfg.renderer = Some(value.to_string()),
            "no_relaunch" => cfg.no_relaunch = parse_bool(value),
            "debug_overlay" => cfg.debug_overlay = parse_bool(value),
            "event_mode" => cfg.event_mode = parse_bool(value),
            "pets" => {
                if let Ok(n) = value.parse::<usize>() {
                    cfg.pets = n.clamp(1, 3);
                }
            }
            _ => {}
        }
    }
    cfg
}

fn print_usage() {
    println!("scamp options:");
    println!("  --list-cats");
    println!("  --cat <gray|ginger|white>");
    println!("  --no-relaunch");
    println!("  --debug-overlay");
    println!("  --pets <1..3>");
    println!("  --event-mode");
    println!("  --renderer <sixel|kitty|halfblock>");
}

fn parse_args_and_apply_env() -> Option<AppConfig> {
    let mut cfg = load_config_file();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_usage();
                return None;
            }
            "--list-cats" => {
                println!("gray\nginger\nwhite");
                return None;
            }
            "--cat" => {
                if let Some(v) = args.next() {
                    cfg.cat = Some(v);
                }
            }
            "--no-relaunch" => cfg.no_relaunch = true,
            "--debug-overlay" => cfg.debug_overlay = true,
            "--event-mode" => cfg.event_mode = true,
            "--pets" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() {
                        cfg.pets = n.clamp(1, 3);
                    }
                }
            }
            "--renderer" => {
                if let Some(v) = args.next() {
                    cfg.renderer = Some(v);
                }
            }
            _ => {}
        }
    }

    if let Ok(v) = std::env::var("SCAMP_CAT") {
        cfg.cat = Some(v);
    }
    if let Ok(v) = std::env::var("SCAMP_RENDERER") {
        cfg.renderer = Some(v);
    } else if let Some(v) = cfg.renderer.clone() {
        std::env::set_var("SCAMP_RENDERER", v);
    }
    if cfg.no_relaunch {
        std::env::set_var("SCAMP_NO_RELAUNCH", "1");
    }
    Some(cfg)
}

fn pick_sheet(cat_override: Option<&str>, offset: usize) -> (&'static [u8], &'static str) {
    use rand::Rng;
    let choice = cat_override.map(|s| s.to_string());
    match choice.as_deref() {
        Some("gray") | Some("grey") => (SHEET_GRAY, "gray"),
        Some("ginger") | Some("orange") | Some("tabby") => (SHEET_GINGER, "ginger"),
        Some("white") => (SHEET_WHITE, "white"),
        _ => {
            let sheets = [
                (SHEET_GRAY, "gray"),
                (SHEET_GINGER, "ginger"),
                (SHEET_WHITE, "white"),
            ];
            let idx = (rand::thread_rng().gen_range(0..sheets.len()) + offset) % sheets.len();
            sheets[idx]
        }
    }
}

const SOURCE_FRAME_PX: u32 = 32;
const SIXEL_TARGET_PX: u32 = 64; // upscaled before sixel encode
const CELL_PIXEL_W: u32 = 9;
const CELL_PIXEL_H: u32 = 18;

const IDLE_FRAMES: &[(u32, u32)] = &[
    (0, 1152),
    (32, 1152),
    (64, 1152),
    (96, 1152),
    (128, 1152),
    (160, 1152),
    (192, 1152),
    (224, 1152),
];
const WALK_RIGHT_FRAMES: &[(u32, u32)] = &[
    (0, 192),
    (32, 192),
    (64, 192),
    (96, 192),
    (128, 192),
    (160, 192),
    (192, 192),
    (224, 192),
];
const WALK_LEFT_FRAMES: &[(u32, u32)] = &[
    (0, 224),
    (32, 224),
    (64, 224),
    (96, 224),
    (128, 224),
    (160, 224),
    (192, 224),
    (224, 224),
];
const WALK_DOWN_FRAMES: &[(u32, u32)] = &[(0, 128), (32, 128), (64, 128), (96, 128)];
const WALK_UP_FRAMES: &[(u32, u32)] = &[(0, 160), (32, 160), (64, 160), (96, 160)];
const SLEEP_CURL_FRAMES: &[(u32, u32)] = &[(0, 512)];
const SLEEP_LOAF_FRAMES: &[(u32, u32)] = &[(0, 384)];
const SLEEP_HEAD_FRAMES: &[(u32, u32)] = &[(0, 448)];
const SLEEP_STRETCH_FRAMES: &[(u32, u32)] = &[(0, 576)];
const YAWN_FRAMES: &[(u32, u32)] = &[
    (0, 1024),
    (32, 1024),
    (64, 1024),
    (96, 1024),
    (128, 1024),
    (160, 1024),
    (192, 1024),
    (224, 1024),
];
const WASH_LIE_FRAMES: &[(u32, u32)] = &[
    (0, 1216),
    (32, 1216),
    (64, 1216),
    (96, 1216),
    (128, 1216),
    (160, 1216),
    (192, 1216),
];
const SCRATCH_FRAMES: &[(u32, u32)] = &[
    (0, 1280),
    (32, 1280),
    (64, 1280),
    (96, 1280),
    (128, 1280),
    (160, 1280),
    (192, 1280),
    (224, 1280),
];
const IDLE_FRAME_MS: u32 = 250;
const WALK_FRAME_MS: u32 = 100;
const SLEEP_FRAME_MS: u32 = 1000;

const ALL_COORDS: [&[(u32, u32)]; 12] = [
    IDLE_FRAMES,
    WALK_RIGHT_FRAMES,
    WALK_LEFT_FRAMES,
    WALK_UP_FRAMES,
    WALK_DOWN_FRAMES,
    SLEEP_CURL_FRAMES,
    YAWN_FRAMES,
    SLEEP_LOAF_FRAMES,
    SLEEP_HEAD_FRAMES,
    SLEEP_STRETCH_FRAMES,
    WASH_LIE_FRAMES,
    SCRATCH_FRAMES,
];
const ALL_DURATIONS: [u32; 12] = [
    IDLE_FRAME_MS,
    WALK_FRAME_MS,
    WALK_FRAME_MS,
    WALK_FRAME_MS,
    WALK_FRAME_MS,
    SLEEP_FRAME_MS,
    IDLE_FRAME_MS,
    SLEEP_FRAME_MS,
    SLEEP_FRAME_MS,
    SLEEP_FRAME_MS,
    IDLE_FRAME_MS,
    IDLE_FRAME_MS,
];

fn pick_shell() -> String {
    if let Ok(s) = std::env::var("SHELL") {
        return s;
    }
    if cfg!(windows) {
        for candidate in ["pwsh.exe", "powershell.exe", "cmd.exe"] {
            if which::which(candidate).is_ok() {
                return candidate.into();
            }
        }
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    } else {
        "/bin/sh".into()
    }
}

fn dump_screen(screen: &Screen) -> std::io::Result<std::path::PathBuf> {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".scamp");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("screen.txt");
    std::fs::write(&path, dump_to_string(screen))?;
    Ok(path)
}

fn build_animations(
    renderer: Renderer,
    cat_override: Option<&str>,
    cat_offset: usize,
) -> anyhow::Result<(Vec<Anim>, u16, u16)> {
    let (sheet_bytes, sheet_name) = pick_sheet(cat_override, cat_offset);
    let renderer_name = match renderer {
        Renderer::Sixel => "sixel",
        Renderer::Kitty => "kitty",
        Renderer::HalfBlock => "half-block",
    };
    eprintln!("[scamp] cat: {} | renderer: {}", sheet_name, renderer_name);
    let img = image::load_from_memory(sheet_bytes)?;

    match renderer {
        Renderer::Sixel | Renderer::Kitty => build_sixel(&img),
        Renderer::HalfBlock => build_halfblock(&img),
    }
}

fn build_sixel(img: &image::DynamicImage) -> anyhow::Result<(Vec<Anim>, u16, u16)> {
    let mut anims = Vec::with_capacity(12);
    for (coords, dur_ms) in ALL_COORDS.iter().zip(ALL_DURATIONS.iter()) {
        let mut sixels = Vec::with_capacity(coords.len());
        for &(x, y) in *coords {
            let region = img.crop_imm(x, y, SOURCE_FRAME_PX, SOURCE_FRAME_PX);
            let scaled = region.resize_exact(
                SIXEL_TARGET_PX,
                SIXEL_TARGET_PX,
                image::imageops::FilterType::Nearest,
            );
            let rgba = scaled.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            sixels.push(sixel::encode_rgba(rgba.as_raw(), w, h)?);
        }
        let durations_ms = vec![*dur_ms; sixels.len()];
        anims.push(Anim::Sixel {
            sixels,
            durations_ms,
        });
    }
    let cell_w = ((SIXEL_TARGET_PX + CELL_PIXEL_W - 1) / CELL_PIXEL_W) as u16;
    let cell_h = ((SIXEL_TARGET_PX + CELL_PIXEL_H - 1) / CELL_PIXEL_H) as u16;
    Ok((anims, cell_w, cell_h))
}

fn build_halfblock(img: &image::DynamicImage) -> anyhow::Result<(Vec<Anim>, u16, u16)> {
    // Half-block render is a per-cell ANSI burst — for a 32×32 source that's
    // ~500 sequences per redraw, which IDE terminals struggle to keep up with
    // (visible flicker on every move). Downscale to 16×16 → ~50 cells, ~10×
    // less work per redraw + a smaller cat to match the fallback aesthetic.
    const HB_TARGET_PX: u32 = 16;
    let mut groups: Vec<Vec<half_block::HbFrame>> = Vec::with_capacity(12);
    for coords in ALL_COORDS.iter() {
        let mut group = Vec::with_capacity(coords.len());
        for &(x, y) in *coords {
            let region = img.crop_imm(x, y, SOURCE_FRAME_PX, SOURCE_FRAME_PX);
            let scaled = region.resize_exact(
                HB_TARGET_PX,
                HB_TARGET_PX,
                image::imageops::FilterType::Lanczos3,
            );
            let rgba = scaled.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            group.push(half_block::frame_from_rgba_bytes(rgba.as_raw(), w, h));
        }
        groups.push(group);
    }
    let groups = half_block::crop_frames_to_union(groups);
    let (cell_w, cell_h) = if !groups.is_empty() && !groups[0].is_empty() {
        (groups[0][0].width_cells, groups[0][0].height_cells)
    } else {
        (1, 1)
    };
    let anims = groups
        .into_iter()
        .zip(ALL_DURATIONS.iter())
        .map(|(frames, &dur_ms)| Anim::HalfBlock {
            durations_ms: vec![dur_ms; frames.len()],
            frames,
        })
        .collect();
    Ok((anims, cell_w, cell_h))
}

fn overlaps_cursor_zone(pet: &Pet, screen: &Screen) -> bool {
    let pr1 = pet.row;
    let pr2 = pet.row.saturating_add(pet.cell_h);
    let pc1 = pet.col;
    let pc2 = pet.col.saturating_add(pet.cell_w);
    let cr = screen.cursor_row;
    let cc = screen.cursor_col;
    cr >= pr1 && cr < pr2 && cc >= pc1 && cc < pc2.saturating_add(2)
}

fn render_debug_overlay(
    stdout: &mut Stdout,
    pets: &[Pet],
    interaction: &InteractionState,
    show_help: bool,
) {
    let mut buf = String::with_capacity(512);
    buf.push_str("\x1b7\x1b[?25l\x1b[1;1H\x1b[0m");
    if show_help {
        buf.push_str("scamp: Ctrl+P pet | Ctrl+F feed | Ctrl+Y help");
    } else {
        let moods = pets
            .iter()
            .map(|p| match p.mood {
                Mood::Happy => "happy",
                Mood::Playful => "playful",
                Mood::Sleepy => "sleepy",
            })
            .collect::<Vec<_>>()
            .join(",");
        let _ = write!(
            buf,
            "pets:{} moods:[{}] pet:{} feed:{}",
            pets.len(),
            moods,
            interaction.pet_presses,
            interaction.feed_presses
        );
    }
    buf.push_str("\x1b8\x1b[?25h");
    let _ = stdout.write_all(buf.as_bytes());
}

fn render_pet(pet: &mut Pet, screen: &Screen, stdout: &mut Stdout) {
    // Pause sprite while a full-screen TUI owns the screen (vim, htop, less).
    if screen.alt_screen {
        pet.was_alt_screen = true;
        // Forget where we were — when alt-screen exits, the terminal will
        // restore the saved buffer and our last_drawn coords would point
        // into a stale, non-existent layout.
        pet.last_drawn = None;
        pet.last_render_scroll = screen.scroll_count;
        return;
    }
    if pet.was_alt_screen {
        // Just left alt-screen; force a clean re-stamp without restoration
        // (terminal restored the saved buffer for us).
        pet.was_alt_screen = false;
        pet.last_drawn = None;
    }

    let now = (pet.row, pet.col, pet.anim_index(), pet.current_frame);
    let scroll_delta_raw = screen.scroll_count.wrapping_sub(pet.last_render_scroll);
    // Cap at screen height — rows scrolled off the top can't have ghosts.
    let scroll_delta = scroll_delta_raw.min(screen.rows as u64) as u16;
    let needs_redraw = pet.last_drawn != Some(now) || scroll_delta > 0;
    if !needs_redraw {
        return;
    }
    if overlaps_cursor_zone(pet, screen) {
        return;
    }

    let mut buf = String::with_capacity(8192);
    buf.push_str("\x1b7\x1b[?25l");

    if let Some((or, oc, _, _)) = pet.last_drawn {
        // Extend restore area upward by scroll_delta — that's where the
        // sprite's pixels physically moved when shell output scrolled.
        let restore_top = or.saturating_sub(scroll_delta);
        let restore_bottom_excl = or.saturating_add(pet.cell_h).min(screen.rows);
        for cy in restore_top..restore_bottom_excl {
            for cx in oc..(oc + pet.cell_w).min(screen.cols) {
                let ch = screen.cell_at(cy, cx).ch;
                let _ = write!(
                    buf,
                    "\x1b[{};{}H\x1b[0m{}",
                    cy + 1,
                    cx + 1,
                    if ch == '\0' { ' ' } else { ch }
                );
            }
        }
    }

    match pet.current_anim() {
        Anim::Sixel { sixels, .. } => {
            let _ = write!(buf, "\x1b[{};{}H", pet.row + 1, pet.col + 1);
            buf.push_str(&sixels[pet.current_frame]);
        }
        Anim::HalfBlock { frames, .. } => {
            half_block::write_frame(
                &mut buf,
                &frames[pet.current_frame],
                pet.row,
                pet.col,
                screen.rows,
                screen.cols,
            );
        }
    }

    buf.push_str("\x1b[0m\x1b8\x1b[?25h");
    let _ = stdout.write_all(buf.as_bytes());
    let _ = stdout.flush();
    pet.last_drawn = Some(now);
    pet.last_render_scroll = screen.scroll_count;
}

fn main() -> anyhow::Result<()> {
    let Some(app_cfg) = parse_args_and_apply_env() else {
        return Ok(());
    };

    // On Windows (especially when launched via double-click in Explorer),
    // the ConHost window can come up without ENABLE_VIRTUAL_TERMINAL_PROCESSING
    // set on the output handle, so all our ANSI codes show up as raw text
    // (left-arrow characters everywhere). Force-enable it before anything writes.
    #[cfg(windows)]
    let _ = enable_ansi_support::enable_ansi_support();

    // Try to relaunch into Windows Terminal on double-click for the sharp
    // sixel experience. No-op if we're already in WT, in another terminal
    // the user explicitly chose, or if wt.exe isn't installed.
    #[cfg(windows)]
    try_relaunch_in_wt();

    let (cols, rows) = size().unwrap_or((100, 30));

    let renderer = detect_renderer();

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let shell = pick_shell();
    let cmd = CommandBuilder::new(shell);
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let master = Arc::new(Mutex::new(pair.master));

    let screen = Arc::new(Mutex::new(Screen::new(cols, rows)));
    let mut pets_vec = Vec::with_capacity(app_cfg.pets);
    for i in 0..app_cfg.pets {
        let (animations, cell_w, cell_h) = build_animations(renderer, app_cfg.cat.as_deref(), i)?;
        let mut pet = Pet::new(rows, cols, animations, cell_w, cell_h);
        if i > 0 {
            let offset = (i as u16).saturating_mul(cell_w.saturating_add(2));
            pet.col = pet.col.saturating_sub(offset);
        }
        pets_vec.push(pet);
    }
    let pets = Arc::new(Mutex::new(pets_vec));
    let interaction = Arc::new(Mutex::new(InteractionState::default()));
    let stdout_lock = Arc::new(Mutex::new(std::io::stdout()));

    enable_raw_mode()?;

    let screen_r = Arc::clone(&screen);
    let stdout_r = Arc::clone(&stdout_lock);
    thread::spawn(move || {
        let mut parser = vte::Parser::new();
        let mut rbuf = [0u8; 4096];
        loop {
            match reader.read(&mut rbuf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Hot path: only screen + stdout. Sprite rendering is
                    // delegated to the tick thread (10fps) so heavy shell
                    // output never gets blocked behind a render burst.
                    // Split into two critical sections so the tick thread
                    // can grab `screen` while we're flushing to stdout, and
                    // vice versa.
                    {
                        let mut s = screen_r.lock().unwrap();
                        for &b in &rbuf[..n] {
                            parser.advance(&mut *s, b);
                        }
                    }
                    let mut out = stdout_r.lock().unwrap();
                    if out.write_all(&rbuf[..n]).is_err() {
                        break;
                    }
                    // Stdout is a LineWriter — without an explicit flush, bytes
                    // without a trailing '\n' (e.g. each keystroke the shell
                    // echoes back) sit in the buffer until the next render
                    // tick flushes. With sleeping-cat frame durations of 1s,
                    // that means up to 1s of input lag.
                    let _ = out.flush();
                }
            }
        }
    });

    let screen_w = Arc::clone(&screen);
    let interaction_w = Arc::clone(&interaction);
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut send_start = 0;
                    for i in 0..n {
                        if buf[i] == DEBUG_DUMP_KEY {
                            if send_start < i && writer.write_all(&buf[send_start..i]).is_err() {
                                return;
                            }
                            let s = screen_w.lock().unwrap();
                            let _ = dump_screen(&s);
                            send_start = i + 1;
                        } else if buf[i] == PET_KEY {
                            let mut it = interaction_w.lock().unwrap();
                            it.pet_presses = it.pet_presses.saturating_add(1);
                            send_start = i + 1;
                        } else if buf[i] == FEED_KEY {
                            let mut it = interaction_w.lock().unwrap();
                            it.feed_presses = it.feed_presses.saturating_add(1);
                            send_start = i + 1;
                        } else if buf[i] == HELP_KEY {
                            let mut it = interaction_w.lock().unwrap();
                            it.help_visible = !it.help_visible;
                            send_start = i + 1;
                        }
                    }
                    if send_start < n && writer.write_all(&buf[send_start..n]).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
            }
        }
    });

    let pets_t = Arc::clone(&pets);
    let screen_t = Arc::clone(&screen);
    let stdout_t = Arc::clone(&stdout_lock);
    let master_t = Arc::clone(&master);
    let interaction_t = Arc::clone(&interaction);
    let event_mode = app_cfg.event_mode;
    let debug_overlay = app_cfg.debug_overlay;
    thread::spawn(move || {
        let mut last_pet_press = 0u64;
        let mut last_feed_press = 0u64;
        loop {
            thread::sleep(Duration::from_millis(TICK_MS));
            // Detect terminal resize each tick. Cheap query, no escape codes.
            if let Ok((cur_cols, cur_rows)) = size() {
                let mut ps = pets_t.lock().unwrap();
                if !ps.is_empty() && (cur_cols, cur_rows) != (ps[0].cols_pub, ps[0].rows_pub) {
                    let mut s = screen_t.lock().unwrap();
                    // Tell the inner shell about the new size so it reflows.
                    let _ = master_t.lock().unwrap().resize(PtySize {
                        rows: cur_rows,
                        cols: cur_cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                    // Reset screen model — the old cell grid is the wrong shape
                    // and the shell will redraw its prompt on next paint anyway.
                    *s = Screen::new(cur_cols, cur_rows);
                    for p in ps.iter_mut() {
                        p.resize(cur_rows, cur_cols);
                    }
                }
            }
            let mut ps = pets_t.lock().unwrap();
            let it = interaction_t.lock().unwrap();
            if it.pet_presses > last_pet_press {
                for p in ps.iter_mut() {
                    p.pet();
                }
                last_pet_press = it.pet_presses;
            }
            if it.feed_presses > last_feed_press {
                for p in ps.iter_mut() {
                    p.feed();
                }
                last_feed_press = it.feed_presses;
            }
            let show_help = it.help_visible;
            let hour = ((std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() / 3600)
                .unwrap_or(12))
                % 24) as u8;
            for p in ps.iter_mut() {
                p.apply_time_bias(hour);
            }
            for i in 0..ps.len() {
                let occupied: Vec<_> = ps
                    .iter()
                    .enumerate()
                    .filter_map(|(j, p)| if j != i { Some(p.bbox()) } else { None })
                    .collect();
                ps[i].tick(&occupied, event_mode);
            }
            let s = screen_t.lock().unwrap();
            let mut out = stdout_t.lock().unwrap();
            for p in ps.iter_mut() {
                render_pet(p, &s, &mut out);
            }
            if debug_overlay || show_help {
                render_debug_overlay(&mut out, &ps, &it, show_help);
                let _ = out.flush();
            }
        }
    });

    let _ = child.wait();

    {
        let p = pets.lock().unwrap();
        let mut stdout = stdout_lock.lock().unwrap();
        for pet in p.iter() {
            if let Some((or, oc, _, _)) = pet.last_drawn {
                let mut buf = String::with_capacity(256);
                let blank = " ".repeat(pet.cell_w as usize);
                for cy in 0..pet.cell_h {
                    let _ = write!(buf, "\x1b[{};{}H\x1b[0m{}", or + cy + 1, oc + 1, blank);
                }
                let _ = stdout.write_all(buf.as_bytes());
            }
        }
        let _ = stdout.flush();
    }

    disable_raw_mode()?;
    let mut stdout = stdout_lock.lock().unwrap();
    let _ = write!(stdout, "\x1b[?25h\r\n");
    let _ = stdout.flush();
    Ok(())
}
