//! The X server's keyboard and pointer: a viewer's keys and pointer put in through XTEST, and
//! the cursor read back through XFixes.
//!
//! VNC names a key by its keysym and X presses keycodes, so a keysym is pressed as the
//! keycode the server's keymap has it on. One the keymap lacks is bound to a keycode nothing
//! uses, the way a keyboard with the key on it would be.
//!
//! The screen file has no cursor in it: the X server draws one only on hardware. [`Cursor`] is
//! its image and where it is, for a viewer to draw or to be drawn into the screen sent.

use std::collections::HashMap;

use x11rb::{
    connection::Connection as _,
    protocol::{
        xfixes::ConnectionExt as _,
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
        conn.xfixes_query_version(4, 0)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| format!("the X server has no XFixes: {e}"))?;
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

    /// The cursor as it is now, or `None` if the X server would not say.
    pub fn cursor(&self) -> Option<Cursor> {
        let image = self.conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
        Some(Cursor {
            x: image.x,
            y: image.y,
            hot: (image.xhot, image.yhot),
            width: image.width,
            height: image.height,
            serial: image.cursor_serial,
            argb: image.cursor_image,
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

/// The cursor: where the pointer is, and the image drawn there.
#[derive(Clone, Debug, PartialEq)]
pub struct Cursor {
    pub x: i16,
    pub y: i16,
    /// The point of the image that is at `x`,`y`.
    pub hot: (u16, u16),
    pub width: u16,
    pub height: u16,
    /// Changes when the image does.
    pub serial: u32,
    /// Premultiplied `0xAARRGGBB`, row after row.
    pub argb: Vec<u32>,
}

impl Cursor {
    /// Each pixel as `0x00RRGGBB`, its alpha taken back out, and whether it is more opaque
    /// than not.
    pub fn pixels(&self) -> impl Iterator<Item = (u32, bool)> + '_ {
        self.argb.iter().map(|&p| {
            let a = p >> 24;
            if a == 0 {
                return (0, false);
            }
            let channel = |shift: u32| (((p >> shift) & 0xff) * 255 / a).min(255) << shift;
            (channel(16) | channel(8) | channel(0), a >= 128)
        })
    }

    /// Draw the cursor into `screen`, `width`×`height` pixels of `0x00RRGGBB`.
    pub fn draw(&self, screen: &mut [u32], width: usize, height: usize) {
        let left = i32::from(self.x) - i32::from(self.hot.0);
        let top = i32::from(self.y) - i32::from(self.hot.1);
        let w = usize::from(self.width);
        for (i, &p) in self.argb.iter().enumerate() {
            let (sx, sy) = (left + (i % w) as i32, top + (i / w) as i32);
            if sx < 0 || sy < 0 || sx as usize >= width || sy as usize >= height {
                continue;
            }
            let at = sy as usize * width + sx as usize;
            let a = p >> 24;
            let below = screen[at];
            let blend = |shift: u32| {
                let src = (p >> shift) & 0xff;
                let dst = (below >> shift) & 0xff;
                (src + dst * (255 - a) / 255).min(255) << shift
            };
            screen[at] = blend(16) | blend(8) | blend(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(argb: Vec<u32>, width: u16) -> Cursor {
        Cursor {
            x: 1,
            y: 1,
            hot: (1, 1),
            width,
            height: (argb.len() / usize::from(width)) as u16,
            serial: 1,
            argb,
        }
    }

    #[test]
    fn a_cursor_is_drawn_at_its_hot_spot_over_what_is_below() {
        // Opaque white, transparent, half-transparent premultiplied black.
        let c = cursor(vec![0xffffffff, 0, 0x80000000, 0], 2);
        let mut screen = vec![0x00ff0000; 9];
        c.draw(&mut screen, 3, 3);
        assert_eq!(screen[0], 0xffffff);
        assert_eq!(screen[1], 0xff0000);
        assert_eq!(screen[3], 0x7f0000);
    }

    #[test]
    fn a_cursor_off_the_screen_is_cut_off_there() {
        let mut c = cursor(vec![0xffffffff; 4], 2);
        (c.x, c.y) = (0, 0);
        let mut screen = vec![0; 4];
        c.draw(&mut screen, 2, 2);
        assert_eq!(screen, [0xffffff, 0, 0, 0]);
    }

    #[test]
    fn pixels_take_the_alpha_back_out() {
        let c = cursor(vec![0x80404040, 0x10101010, 0], 3);
        let pixels: Vec<_> = c.pixels().collect();
        assert_eq!(pixels, [(0x7f7f7f, true), (0xffffff, false), (0, false)]);
    }
}
