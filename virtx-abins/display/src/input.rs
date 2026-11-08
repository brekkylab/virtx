//! A viewer's keys and pointer, put into the X server through XTEST.
//!
//! VNC names a key by its keysym and X presses keycodes, so a keysym is pressed as the
//! keycode the server's keymap has it on. One the keymap lacks is bound to a keycode nothing
//! uses, the way a keyboard with the key on it would be.

use std::collections::HashMap;

use x11rb::{
    connection::Connection as _,
    protocol::{
        xproto::{self, ConnectionExt as _},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};

/// The pointer buttons a VNC button mask has, 1 to 8.
const BUTTONS: u8 = 8;

pub struct Input {
    conn: RustConnection,
    root: xproto::Window,
    min_keycode: u8,
    per_keycode: u8,
    /// The keysyms on each keycode, from `min_keycode`.
    keymap: Vec<u32>,
    bound: HashMap<u32, u8>,
    buttons: u8,
}

impl Input {
    pub fn connect(display: u16) -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(Some(&format!(":{display}")))
            .map_err(|e| format!("connecting to :{display}: {e}"))?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| format!("the X server has no XTEST: {e}"))?;
        let (min_keycode, max_keycode) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let mapping = conn
            .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| format!("reading the keymap: {e}"))?;
        Ok(Self {
            conn,
            root,
            min_keycode,
            per_keycode: mapping.keysyms_per_keycode,
            keymap: mapping.keysyms,
            bound: HashMap::new(),
            buttons: 0,
        })
    }

    pub fn key(&mut self, keysym: u32, down: bool) {
        let Some(keycode) = self.keycode(keysym) else {
            return;
        };
        let kind = if down {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.fake(kind, keycode, 0, 0);
    }

    /// Move the pointer to `x`,`y` and press or release the buttons `mask` changes.
    pub fn pointer(&mut self, mask: u8, x: u16, y: u16) {
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x as i16, y as i16);
        for button in 0..BUTTONS {
            let bit = 1 << button;
            if (mask ^ self.buttons) & bit == 0 {
                continue;
            }
            let kind = if mask & bit != 0 {
                xproto::BUTTON_PRESS_EVENT
            } else {
                xproto::BUTTON_RELEASE_EVENT
            };
            self.fake(kind, button + 1, 0, 0);
        }
        self.buttons = mask;
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        let _ = self
            .conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0);
        let _ = self.conn.flush();
    }

    fn keycode(&mut self, keysym: u32) -> Option<u8> {
        if let Some(&keycode) = self.bound.get(&keysym) {
            return Some(keycode);
        }
        let per = usize::from(self.per_keycode);
        let keycodes = self.keymap.chunks_exact(per);
        if let Some(i) = keycodes.clone().position(|syms| syms.contains(&keysym)) {
            return Some(self.min_keycode + i as u8);
        }
        // A keycode with nothing on it, given the keysym on every level.
        let i = keycodes
            .clone()
            .position(|syms| syms.iter().all(|&s| s == 0))?;
        let keycode = self.min_keycode + i as u8;
        let syms = vec![keysym; per];
        self.conn
            .change_keyboard_mapping(1, keycode, self.per_keycode, &syms)
            .ok()?;
        self.conn.get_input_focus().ok()?.reply().ok()?;
        self.keymap[i * per..(i + 1) * per].copy_from_slice(&syms);
        self.bound.insert(keysym, keycode);
        Some(keycode)
    }
}
