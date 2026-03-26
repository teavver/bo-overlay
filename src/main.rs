use std::{env, fs, path::PathBuf, process, thread, time::Duration};

use as_raw_xcb_connection::AsRawXcbConnection;
use cairo::{XCBConnection as CairoConn, XCBDrawable, XCBSurface, XCBVisualType};
use serde::Deserialize;
use x11rb::connection::Connection;
use x11rb::protocol::shape::{ConnectionExt as ShapeExt, SK, SO};
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    /// Font size in pixels. Leave unset to auto-calculate (1.5% of screen height).
    font_size: Option<f64>,
    /// Font family. "Monospace", "Sans", or any system font name.
    font_family: String,
    /// Wrap text when the window is narrower than the content.
    /// When false, text is clipped at the window edge.
    wrap: bool,
    /// Key binding to toggle overlay/active mode. E.g. "ctrl-t", "ctrl-shift-f".
    keybind: String,
    /// Text color in overlay mode (mouse not over window). "#RRGGBBAA" hex.
    color_idle: String,
    /// Text color in overlay mode when mouse is hovering. "#RRGGBBAA" hex.
    color_hover: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_size: None,
            font_family: "Monospace".into(),
            wrap: true,
            keybind: "ctrl-t".into(),
            color_idle: "#FFFFFFBF".into(),  // white, 75% opaque
            color_hover: "#FFFFFF40".into(), // white, 25% opaque
        }
    }
}

fn load_config(path: &PathBuf) -> Config {
    match fs::read_to_string(path) {
        Ok(s) => {
            eprintln!("Config: {}", path.display());
            toml::from_str(&s).unwrap_or_else(|e| {
                eprintln!("Warning: could not parse config: {e}");
                Config::default()
            })
        }
        Err(_) => {
            eprintln!("Config: {} (not found, using defaults)", path.display());
            Config::default()
        }
    }
}

/// Parse a color string into (r, g, b, a) as 0.0–1.0. Accepted formats:
///   "#RRGGBBAA", "#RRGGBB"
///   "rgba(255, 255, 255, 0.75)", "rgb(255, 255, 255)"
fn parse_color(s: &str) -> Option<(f64, f64, f64, f64)> {
    let s = s.trim();

    // hex
    if s.starts_with('#') {
        let h = &s[1..];
        let parse = |slice: &str| u8::from_str_radix(slice, 16).ok().map(|v| v as f64 / 255.0);
        return match h.len() {
            8 => Some((parse(&h[0..2])?, parse(&h[2..4])?, parse(&h[4..6])?, parse(&h[6..8])?)),
            6 => Some((parse(&h[0..2])?, parse(&h[2..4])?, parse(&h[4..6])?, 1.0)),
            _ => None,
        };
    }

    // rgba(...) / rgb(...)
    let s_low = s.to_ascii_lowercase();
    let (inner, has_alpha) = if let Some(inner) = s_low.strip_prefix("rgba(").and_then(|s| s.strip_suffix(')')) {
        (inner, true)
    } else if let Some(inner) = s_low.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        (inner, false)
    } else {
        return None;
    };

    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    let expected = if has_alpha { 4 } else { 3 };
    if parts.len() != expected { return None; }

    let r = parts[0].parse::<f64>().ok()? / 255.0;
    let g = parts[1].parse::<f64>().ok()? / 255.0;
    let b = parts[2].parse::<f64>().ok()? / 255.0;
    let a = if has_alpha { parts[3].parse::<f64>().ok()? } else { 1.0 };

    Some((r, g, b, a))
}

/// Parse "ctrl-t", "ctrl-shift-f2" etc. into (ModMask, keysym).
fn parse_keybind(s: &str) -> Result<(ModMask, u32), String> {
    let lower = s.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('-').collect();
    if parts.is_empty() {
        return Err(format!("empty keybind: {s}"));
    }
    let key_str = *parts.last().unwrap();
    let mod_strs = &parts[..parts.len() - 1];

    let mut raw: u16 = 0;
    for m in mod_strs {
        raw |= match *m {
            "ctrl" | "control" => u16::from(ModMask::CONTROL),
            "shift"            => u16::from(ModMask::SHIFT),
            "alt"              => u16::from(ModMask::M1),
            "super" | "mod4"   => u16::from(ModMask::M4),
            other => return Err(format!("unknown modifier: {other}")),
        };
    }

    let keysym: u32 = if key_str.len() == 1 {
        key_str.chars().next().unwrap() as u32
    } else {
        match key_str {
            "f1"  => 0xffbe, "f2"  => 0xffbf, "f3"  => 0xffc0,
            "f4"  => 0xffc1, "f5"  => 0xffc2, "f6"  => 0xffc3,
            "f7"  => 0xffc4, "f8"  => 0xffc5, "f9"  => 0xffc6,
            "f10" => 0xffc7, "f11" => 0xffc8, "f12" => 0xffc9,
            "space"           => 0x0020,
            "return" | "enter"=> 0xff0d,
            "tab"             => 0xff09,
            "escape" | "esc"  => 0xff1b,
            other => return Err(format!("unknown key: {other}")),
        }
    };

    Ok((ModMask::from(raw), keysym))
}

// ---------------------------------------------------------------------------
// X11 / window boilerplate
// ---------------------------------------------------------------------------

/// Matches xcb_visualtype_t layout exactly so we can hand a pointer to cairo.
#[repr(C)]
struct RawVisualType {
    visual_id: u32,
    class: u8,
    bits_per_rgb_value: u8,
    colormap_entries: u16,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    _pad: [u8; 4],
}

fn main() {
    let raw_args: Vec<String> = env::args().collect();

    // Split into flags (--config=...) and positional args.
    let mut config_path: Option<PathBuf> = None;
    let mut positional: Vec<&str> = Vec::new();
    for arg in &raw_args[1..] {
        if let Some(val) = arg.strip_prefix("--config=") {
            config_path = Some(PathBuf::from(val));
        } else {
            positional.push(arg.as_str());
        }
    }

    if positional.is_empty() {
        eprintln!("Usage: {} <file> [--config=<path>]", raw_args[0]);
        eprintln!();
        eprintln!("  Displays text from <file> as a transparent X11 overlay.");
        eprintln!("  Default config: ~/.bo-config.toml");
        process::exit(1);
    }

    let file_path = positional[0];
    let content = match fs::read_to_string(file_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading '{}': {}", file_path, e);
            process::exit(1);
        }
    };
    let filename = PathBuf::from(file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_path.to_string());

    let config_path = config_path.unwrap_or_else(|| {
        PathBuf::from(env::var("HOME").unwrap_or_default()).join(".bo-config.toml")
    });

    let cfg = load_config(&config_path);

    let (mod_mask, keysym) = parse_keybind(&cfg.keybind).unwrap_or_else(|e| {
        eprintln!("Warning: invalid keybind '{}': {e}. Falling back to ctrl-t.", cfg.keybind);
        (ModMask::CONTROL, b't' as u32)
    });

    let color_idle = parse_color(&cfg.color_idle).unwrap_or_else(|| {
        eprintln!("Warning: invalid color_idle '{}'. Using default.", cfg.color_idle);
        (1.0, 1.0, 1.0, 0.75)
    });
    let color_hover = parse_color(&cfg.color_hover).unwrap_or_else(|| {
        eprintln!("Warning: invalid color_hover '{}'. Using default.", cfg.color_hover);
        (1.0, 1.0, 1.0, 0.25)
    });

    if let Err(e) = run(&content, &filename, &cfg, mod_mask, keysym, color_idle, color_hover) {
        eprintln!("Fatal: {e}");
        process::exit(1);
    }
}

fn run(
    content: &str,
    filename: &str,
    cfg: &Config,
    mod_mask: ModMask,
    keysym: u32,
    color_idle: (f64, f64, f64, f64),
    color_hover: (f64, f64, f64, f64),
) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen_num) = XCBConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let font_px = cfg.font_size
        .unwrap_or_else(|| (screen.height_in_pixels as f64 * 0.015).round());

    let (visual_id, raw_visual) = find_argb_visual(&conn, screen_num)
        .ok_or("No 32-bit ARGB visual found. Is a compositor running?")?;

    let (text_w, text_h) = measure_text(content, font_px, &cfg.font_family);
    let bar_h = (font_px as i32 + 20) as u32; // PAD*2 = 10*2
    let nat_w = (text_w + 20) as u32;
    let content_h = (text_h + 20) as u32;

    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, root, visual_id)?;

    let win = conn.generate_id()?;
    conn.create_window(
        32, win, root, 50, 50,
        nat_w as u16, (content_h + bar_h) as u16,
        0, WindowClass::INPUT_OUTPUT, visual_id,
        &CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            .event_mask(
                EventMask::EXPOSURE | EventMask::KEY_PRESS | EventMask::STRUCTURE_NOTIFY,
            ),
    )?;

    let a_wm_del    = intern_atom(&conn, b"WM_DELETE_WINDOW")?;
    let a_wm_proto  = intern_atom(&conn, b"WM_PROTOCOLS")?;
    let a_state     = intern_atom(&conn, b"_NET_WM_STATE")?;
    let a_above     = intern_atom(&conn, b"_NET_WM_STATE_ABOVE")?;
    let a_skip_bar  = intern_atom(&conn, b"_NET_WM_STATE_SKIP_TASKBAR")?;
    let a_skip_pager= intern_atom(&conn, b"_NET_WM_STATE_SKIP_PAGER")?;
    let a_wm_type   = intern_atom(&conn, b"_NET_WM_WINDOW_TYPE")?;
    let a_splash    = intern_atom(&conn, b"_NET_WM_WINDOW_TYPE_SPLASH")?;
    let a_motif     = intern_atom(&conn, b"_MOTIF_WM_HINTS")?;

    conn.change_property32(PropMode::REPLACE, win, a_wm_proto, AtomEnum::ATOM, &[a_wm_del])?;
    conn.change_property32(PropMode::REPLACE, win, a_state, AtomEnum::ATOM,
        &[a_above, a_skip_bar, a_skip_pager])?;
    conn.change_property32(PropMode::REPLACE, win, a_wm_type, AtomEnum::ATOM, &[a_splash])?;
    conn.change_property32(PropMode::REPLACE, win, a_motif, a_motif, &[2u32, 0, 0, 0, 0])?;
    conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"bo-overlay")?;

    conn.map_window(win)?;
    conn.flush()?;

    let cairo_conn = unsafe { CairoConn::from_raw_none(conn.as_raw_xcb_connection() as *mut _) };
    let cairo_visual = unsafe {
        XCBVisualType::from_raw_none(&raw_visual as *const RawVisualType as *mut _)
    };
    let surface = XCBSurface::create(
        &cairo_conn, &XCBDrawable(win), &cairo_visual, nat_w as i32, (content_h + bar_h) as i32,
    )?;

    let kc = keycode_for_sym(&conn, keysym)
        .ok_or("Could not find keycode for configured keybind key")?;
    conn.grab_key(false, root, mod_mask, kc, GrabMode::ASYNC, GrabMode::ASYNC)?.check()?;

    let mut overlay = false;
    let mut hovered = false;
    let mut cur_w   = nat_w;
    let mut cur_h   = content_h + bar_h;

    set_click_through(&conn, win, false)?;
    draw(&surface, content, filename, cur_w, nat_w, font_px, &cfg.font_family, cfg.wrap,
         false, bar_h, false, color_idle, color_hover);
    conn.flush()?;

    loop {
        conn.flush()?;

        if overlay {
            let ptr = conn.query_pointer(win)?.reply()?;
            let over = ptr.win_x >= 0 && ptr.win_y >= 0
                && (ptr.win_x as u32) < cur_w
                && (ptr.win_y as u32) < cur_h;
            if over != hovered {
                hovered = over;
                draw(&surface, content, filename, cur_w, nat_w, font_px, &cfg.font_family, cfg.wrap,
                     overlay, bar_h, hovered, color_idle, color_hover);
                conn.flush()?;
            }
        }

        match conn.poll_for_event()? {
            None => { thread::sleep(Duration::from_millis(16)); continue; }
            Some(event) => match event {
                Event::Expose(e) if e.count == 0 => {
                    draw(&surface, content, filename, cur_w, nat_w, font_px, &cfg.font_family, cfg.wrap,
                         overlay, bar_h, hovered, color_idle, color_hover);
                }
                Event::KeyPress(e) if e.detail == kc => {
                    overlay = !overlay;
                    hovered = false;
                    set_click_through(&conn, win, overlay)?;
                    // No configure_window — window size is never changed by the app.
                    draw(&surface, content, filename, cur_w, nat_w, font_px, &cfg.font_family, cfg.wrap,
                         overlay, bar_h, hovered, color_idle, color_hover);
                    conn.flush()?;
                    let _ = e.time;
                }
                Event::ConfigureNotify(e) if e.window == win => {
                    let nw = e.width as u32;
                    let nh = e.height as u32;
                    if nw != cur_w || nh != cur_h {
                        cur_w = nw;
                        cur_h = nh;
                        surface.set_size(nw as i32, nh as i32)?;
                        draw(&surface, content, filename, cur_w, nat_w, font_px, &cfg.font_family, cfg.wrap,
                             overlay, bar_h, hovered, color_idle, color_hover);
                    }
                }
                Event::ClientMessage(e) if e.data.as_data32()[0] == a_wm_del => break,
                _ => {}
            },
        }
    }

    conn.ungrab_key(kc, root, mod_mask)?.check()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_argb_visual(conn: &XCBConnection, screen_num: usize) -> Option<(u32, RawVisualType)> {
    for depth in &conn.setup().roots[screen_num].allowed_depths {
        if depth.depth != 32 { continue; }
        if let Some(vis) = depth.visuals.first() {
            return Some((vis.visual_id, RawVisualType {
                visual_id: vis.visual_id,
                class: 4,
                bits_per_rgb_value: vis.bits_per_rgb_value,
                colormap_entries: vis.colormap_entries,
                red_mask: vis.red_mask,
                green_mask: vis.green_mask,
                blue_mask: vis.blue_mask,
                _pad: [0; 4],
            }));
        }
    }
    None
}

fn intern_atom(conn: &XCBConnection, name: &[u8]) -> Result<u32, Box<dyn std::error::Error>> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn keycode_for_sym(conn: &XCBConnection, sym: u32) -> Option<u8> {
    let setup = conn.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode - first + 1;
    let map = conn.get_keyboard_mapping(first, count).ok()?.reply().ok()?;
    let spc = map.keysyms_per_keycode as usize;
    for (i, chunk) in map.keysyms.chunks(spc).enumerate() {
        if chunk.contains(&sym) { return Some(first + i as u8); }
    }
    None
}

fn set_click_through(conn: &XCBConnection, win: u32, enabled: bool)
    -> Result<(), Box<dyn std::error::Error>>
{
    if enabled {
        conn.shape_rectangles(SO::SET, SK::INPUT, ClipOrdering::UNSORTED, win, 0, 0, &[])?.check()?;
    } else {
        conn.shape_mask(SO::SET, SK::INPUT, win, 0, 0, 0u32)?.check()?;
    }
    Ok(())
}

fn font_desc(font_px: f64, family: &str) -> pango::FontDescription {
    let mut fd = pango::FontDescription::new();
    fd.set_family(family);
    fd.set_absolute_size(font_px * pango::SCALE as f64);
    fd
}

fn measure_text(content: &str, font_px: f64, family: &str) -> (i32, i32) {
    let surf = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).unwrap();
    let cr = cairo::Context::new(&surf).unwrap();
    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_font_description(Some(&font_desc(font_px, family)));
    layout.set_width(-1);
    layout.set_text(content);
    layout.pixel_size()
}

#[allow(clippy::too_many_arguments)]
fn draw(
    surface: &XCBSurface,
    content: &str,
    filename: &str,
    win_w: u32,
    nat_w: u32,
    font_px: f64,
    family: &str,
    wrap: bool,
    overlay: bool,
    bar_h: u32,
    hovered: bool,
    color_idle: (f64, f64, f64, f64),
    color_hover: (f64, f64, f64, f64),
) {
    let cr = match cairo::Context::new(surface) { Ok(c) => c, Err(_) => return };
    const PAD: f64 = 10.0;

    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    let _ = cr.paint();

    if !overlay {
        cr.set_source_rgba(40.0 / 255.0, 85.0 / 255.0, 119.0 / 255.0, 1.0);
        cr.rectangle(0.0, 0.0, win_w as f64, bar_h as f64);
        let _ = cr.fill();

        let bl = pangocairo::functions::create_layout(&cr);
        bl.set_font_description(Some(&font_desc(font_px, family)));
        bl.set_text(&format!("normal ~ {filename}"));
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let (_, lh) = bl.pixel_size();
        cr.move_to(PAD, (bar_h as f64 - lh as f64) / 2.0);
        pangocairo::functions::show_layout(&cr, &bl);
    }

    let (r, g, b, a) = if overlay {
        if hovered { color_hover } else { color_idle }
    } else {
        (1.0, 1.0, 1.0, 1.0)
    };

    let y_off = if overlay { PAD } else { bar_h as f64 + PAD };

    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_font_description(Some(&font_desc(font_px, family)));

    if wrap && win_w < nat_w {
        layout.set_width((win_w as i32 - 20) * pango::SCALE);
        layout.set_wrap(pango::WrapMode::WordChar);
    } else {
        layout.set_width(-1);
    }

    layout.set_text(content);

    cr.set_operator(cairo::Operator::Over);
    cr.set_source_rgba(r, g, b, a);
    cr.move_to(PAD, y_off);
    pangocairo::functions::show_layout(&cr, &layout);

    surface.flush();
}
