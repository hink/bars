//! Character -> key mapping for typing through the virtual keyboard.
//!
//! Key codes sent through uinput are interpreted by the compositor with the
//! user's active layout, so to type "é" on a French layout we must know that
//! layout. We ask the Wayland compositor for its exact keymap (any client
//! receives it on `wl_keyboard`), falling back to RMLVO names on X11.

use std::collections::HashMap;
use std::os::unix::fs::FileExt;

use evdev::KeyCode;
use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use xkbcommon::xkb;

/// Which modifiers a character needs on top of its key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub key: KeyCode,
    pub shift: bool,
    pub altgr: bool,
}

pub struct CharMap {
    map: HashMap<char, Stroke>,
}

impl CharMap {
    pub fn load() -> CharMap {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = wayland_keymap()
            .and_then(|s| {
                xkb::Keymap::new_from_string(
                    &ctx,
                    s,
                    xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkb::KEYMAP_COMPILE_NO_FLAGS,
                )
            })
            .or_else(|| {
                let layout = std::env::var("XKB_DEFAULT_LAYOUT").unwrap_or_default();
                xkb::Keymap::new_from_names(&ctx, "", "", &layout, "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
            });
        let mut map = HashMap::new();
        if let Some(km) = keymap {
            let layout = 0;
            for raw in km.min_keycode().raw()..=km.max_keycode().raw() {
                let kc = xkb::Keycode::new(raw);
                let levels = km.num_levels_for_key(kc, layout).min(4);
                for level in 0..levels {
                    for sym in km.key_get_syms_by_level(kc, layout, level) {
                        let cp = xkb::keysym_to_utf32(*sym);
                        let Some(ch) = char::from_u32(cp).filter(|c| *c != '\0') else {
                            continue;
                        };
                        // Keep the simplest way to type each character.
                        map.entry(ch).or_insert(Stroke {
                            key: KeyCode::new((raw - 8) as u16),
                            shift: level == 1 || level == 3,
                            altgr: level >= 2,
                        });
                    }
                }
            }
        }
        if map.is_empty() {
            log::warn!("no keymap available; typing will be limited");
        }
        CharMap { map }
    }

    pub fn stroke(&self, c: char) -> Option<Stroke> {
        self.map.get(&c).copied()
    }

    pub fn can_type(&self, text: &str) -> bool {
        text.chars().all(|c| c == '\n' || c == '\t' || self.map.contains_key(&c))
    }
}

#[derive(Default)]
struct KeymapState {
    keymap: Option<String>,
    done: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for KeymapState {
    fn event(
        _: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == "wl_seat"
        {
            registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(5), qh, ());
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for KeymapState {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event
            && caps.contains(wl_seat::Capability::Keyboard)
        {
            seat.get_keyboard(qh, ());
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for KeymapState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } = event {
            let file = std::fs::File::from(fd);
            let mut buf = vec![0u8; size as usize];
            if file.read_exact_at(&mut buf, 0).is_ok() {
                // The keymap is NUL-terminated.
                while buf.last() == Some(&0) {
                    buf.pop();
                }
                state.keymap = String::from_utf8(buf).ok();
            }
            state.done = true;
        }
    }
}

fn wayland_keymap() -> Option<String> {
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = KeymapState::default();
    // registry -> seat -> keyboard -> keymap: a few roundtrips at most.
    for _ in 0..4 {
        queue.roundtrip(&mut state).ok()?;
        if state.done {
            break;
        }
    }
    state.keymap
}
