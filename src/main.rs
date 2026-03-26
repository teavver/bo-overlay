use std::{env, fs, process, thread, time::Duration};

use as_raw_xcb_connection::AsRawXcbConnection;
use cairo::{XCBConnection as CairoConn, XCBDrawable, XCBSurface, XCBVisualType};
use x11rb::connection::Connection;
use x11rb::protocol::shape::{ConnectionExt as ShapeExt, SK, SO};
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

const FONT_SCALE: f64 = 0.015;
const PAD: i32 = 10;

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
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <file>", args[0]);
        eprintln!();
        eprintln!("  Displays text from <file> as a transparent X11 overlay.");
        eprintln!("  Ctrl+T  toggle between overlay (click-through) and active (interactive) mode.");
        process::exit(1);
    }

    let content = match fs::read_to_string(&args[1]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading '{}': {}", args[1], e);
            process::exit(1);
        }
    };

    if let Err(e) = run(&content) {
        eprintln!("Fatal: {}", e);
        process::exit(1);
    }
}

fn run(content: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen_num) = XCBConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let font_px = (screen.height_in_pixels as f64 * FONT_SCALE).round();

    let (visual_id, raw_visual) = find_argb_visual(&conn, screen_num)
        .ok_or("No 32-bit ARGB visual found. Is a compositor (picom, compton, etc.) running?")?;

    let (text_w, text_h) = measure_text(content, font_px);
    let bar_h = (font_px as i32 + PAD * 2) as u32;
    let nat_w = (text_w + PAD * 2) as u32;
    // content_h is the height without the bar; bar_h is added when entering active mode.
    let content_h = (text_h + PAD * 2) as u32;

    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, root, visual_id)?;

    let win = conn.generate_id()?;
    conn.create_window(
        32,
        win,
        root,
        50, 50,
        nat_w as u16,
        content_h as u16, // start in overlay mode: no bar
        0,
        WindowClass::INPUT_OUTPUT,
        visual_id,
        &CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            .event_mask(
                EventMask::EXPOSURE | EventMask::KEY_PRESS | EventMask::STRUCTURE_NOTIFY,
            ),
    )?;

    let a_wm_del = intern_atom(&conn, b"WM_DELETE_WINDOW")?;
    let a_wm_proto = intern_atom(&conn, b"WM_PROTOCOLS")?;
    let a_state = intern_atom(&conn, b"_NET_WM_STATE")?;
    let a_above = intern_atom(&conn, b"_NET_WM_STATE_ABOVE")?;
    let a_skip_bar = intern_atom(&conn, b"_NET_WM_STATE_SKIP_TASKBAR")?;
    let a_skip_pager = intern_atom(&conn, b"_NET_WM_STATE_SKIP_PAGER")?;

    conn.change_property32(PropMode::REPLACE, win, a_wm_proto, AtomEnum::ATOM, &[a_wm_del])?;
    conn.change_property32(
        PropMode::REPLACE, win, a_state, AtomEnum::ATOM,
        &[a_above, a_skip_bar, a_skip_pager],
    )?;
    conn.change_property8(
        PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"bo-overlay",
    )?;

    // Remove all WM decorations (title bar + border) via Motif hints.
    // flags=2 means the decorations field is active; decorations=0 means none.
    let a_motif = intern_atom(&conn, b"_MOTIF_WM_HINTS")?;
    conn.change_property32(
        PropMode::REPLACE, win, a_motif, a_motif, &[2u32, 0, 0, 0, 0],
    )?;

    // SPLASH: i3 floats it, applies no border (BS_NONE), and does not tile it.
    let a_wm_type = intern_atom(&conn, b"_NET_WM_WINDOW_TYPE")?;
    let a_wm_type_splash = intern_atom(&conn, b"_NET_WM_WINDOW_TYPE_SPLASH")?;
    conn.change_property32(
        PropMode::REPLACE, win, a_wm_type, AtomEnum::ATOM, &[a_wm_type_splash],
    )?;

    conn.map_window(win)?;
    conn.flush()?;

    let cairo_conn = unsafe {
        CairoConn::from_raw_none(conn.as_raw_xcb_connection() as *mut _)
    };
    let cairo_visual = unsafe {
        XCBVisualType::from_raw_none(&raw_visual as *const RawVisualType as *mut _)
    };
    let surface = XCBSurface::create(
        &cairo_conn, &XCBDrawable(win), &cairo_visual, nat_w as i32, content_h as i32,
    )?;

    let kc_t = keycode_for_sym(&conn, b't' as u32)
        .ok_or("Could not find keycode for 't'")?;
    conn.grab_key(
        false, root, ModMask::CONTROL, kc_t, GrabMode::ASYNC, GrabMode::ASYNC,
    )?.check()?;

    let mut overlay = true;
    let mut hovered = false;
    set_click_through(&conn, win, true)?;
    draw(&surface, content, nat_w, nat_w, font_px, true, bar_h, false);
    conn.flush()?;

    let mut cur_w = nat_w;
    let mut cur_h = content_h;
    // base_h is the content-only height (no bar). Always derive window height from this,
    // never from cur_h, to avoid compounding errors when toggling quickly.
    let mut base_h = content_h;

    loop {
        conn.flush()?;

        // In overlay mode, poll pointer position to detect hover and adjust text opacity.
        // The window is click-through so it never receives pointer events directly.
        if overlay {
            let ptr = conn.query_pointer(win)?.reply()?;
            let over = ptr.win_x >= 0 && ptr.win_y >= 0
                && (ptr.win_x as u32) < cur_w
                && (ptr.win_y as u32) < cur_h;
            if over != hovered {
                hovered = over;
                draw(&surface, content, cur_w, nat_w, font_px, overlay, bar_h, hovered);
                conn.flush()?;
            }
        }

        match conn.poll_for_event()? {
            None => {
                thread::sleep(Duration::from_millis(16));
                continue;
            }
            Some(event) => match event {
                Event::Expose(e) if e.count == 0 => {
                    draw(&surface, content, cur_w, nat_w, font_px, overlay, bar_h, hovered);
                }
                Event::KeyPress(e) if e.detail == kc_t => {
                    overlay = !overlay;
                    hovered = false;
                    set_click_through(&conn, win, overlay)?;
                    let new_h = if overlay { base_h } else { base_h + bar_h };
                    conn.configure_window(win, &ConfigureWindowAux::new().height(new_h))?.check()?;
                    draw(&surface, content, cur_w, nat_w, font_px, overlay, bar_h, hovered);
                    conn.flush()?;
                    let _ = e.time; // suppress unused warning
                }
                Event::ConfigureNotify(e) if e.window == win => {
                    let nw = e.width as u32;
                    let nh = e.height as u32;
                    if nw != cur_w || nh != cur_h {
                        cur_w = nw;
                        cur_h = nh;
                        // Keep base_h in sync with user-driven resizes.
                        base_h = if overlay { nh } else { nh.saturating_sub(bar_h).max(content_h) };
                        surface.set_size(nw as i32, nh as i32)?;
                        draw(&surface, content, cur_w, nat_w, font_px, overlay, bar_h, hovered);
                    }
                }
                Event::ClientMessage(e) if e.data.as_data32()[0] == a_wm_del => break,
                _ => {}
            },
        }
    }

    conn.ungrab_key(kc_t, root, ModMask::CONTROL)?.check()?;
    Ok(())
}

// --- helpers -----------------------------------------------------------------

fn find_argb_visual(conn: &XCBConnection, screen_num: usize) -> Option<(u32, RawVisualType)> {
    for depth in &conn.setup().roots[screen_num].allowed_depths {
        if depth.depth != 32 {
            continue;
        }
        if let Some(vis) = depth.visuals.first() {
            let rv = RawVisualType {
                visual_id: vis.visual_id,
                class: 4,
                bits_per_rgb_value: vis.bits_per_rgb_value,
                colormap_entries: vis.colormap_entries,
                red_mask: vis.red_mask,
                green_mask: vis.green_mask,
                blue_mask: vis.blue_mask,
                _pad: [0; 4],
            };
            return Some((vis.visual_id, rv));
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
        if chunk.contains(&sym) {
            return Some(first + i as u8);
        }
    }
    None
}

fn set_click_through(
    conn: &XCBConnection,
    win: u32,
    enabled: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if enabled {
        conn.shape_rectangles(
            SO::SET, SK::INPUT, ClipOrdering::UNSORTED, win, 0, 0, &[],
        )?.check()?;
    } else {
        conn.shape_mask(SO::SET, SK::INPUT, win, 0, 0, 0u32)?.check()?;
    }
    Ok(())
}

fn measure_text(content: &str, font_px: f64) -> (i32, i32) {
    let surf = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1).unwrap();
    let cr = cairo::Context::new(&surf).unwrap();
    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_font_description(Some(&monospace_desc(font_px)));
    layout.set_width(-1);
    layout.set_text(content);
    layout.pixel_size()
}

fn monospace_desc(font_px: f64) -> pango::FontDescription {
    let mut fd = pango::FontDescription::new();
    fd.set_family("Monospace");
    fd.set_absolute_size(font_px * pango::SCALE as f64);
    fd
}

fn draw(
    surface: &XCBSurface,
    content: &str,
    win_w: u32,
    nat_w: u32,
    font_px: f64,
    overlay: bool,
    bar_h: u32,
    hovered: bool,
) {
    let cr = match cairo::Context::new(surface) {
        Ok(c) => c,
        Err(_) => return,
    };

    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    let _ = cr.paint();

    if !overlay {
        // i3wm default focused title bar: #285577
        cr.set_source_rgba(40.0 / 255.0, 85.0 / 255.0, 119.0 / 255.0, 1.0);
        cr.rectangle(0.0, 0.0, win_w as f64, bar_h as f64);
        let _ = cr.fill();

        let bar_layout = pangocairo::functions::create_layout(&cr);
        bar_layout.set_font_description(Some(&monospace_desc(font_px)));
        bar_layout.set_text("normal");
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let (_, label_h) = bar_layout.pixel_size();
        cr.move_to(PAD as f64, (bar_h as f64 - label_h as f64) / 2.0);
        pangocairo::functions::show_layout(&cr, &bar_layout);
    }

    // Text alpha: fully opaque in active mode; 25% opaque idle, 75% opaque on hover in overlay.
    let text_alpha: f64 = if !overlay {
        1.0
    } else if hovered {
        0.25
    } else {
        0.75
    };

    let y_off: f64 = if overlay { PAD as f64 } else { bar_h as f64 + PAD as f64 };

    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_font_description(Some(&monospace_desc(font_px)));

    if win_w < nat_w {
        layout.set_width((win_w as i32 - PAD * 2) * pango::SCALE);
        layout.set_wrap(pango::WrapMode::WordChar);
    } else {
        layout.set_width(-1);
    }

    layout.set_text(content);

    cr.set_operator(cairo::Operator::Over);
    cr.set_source_rgba(1.0, 1.0, 1.0, text_alpha);
    cr.move_to(PAD as f64, y_off);
    pangocairo::functions::show_layout(&cr, &layout);

    surface.flush();
}
