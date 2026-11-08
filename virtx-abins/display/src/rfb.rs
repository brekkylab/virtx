//! The VNC server: RFB 3.8 (RFC 6143), with 3.3 and 3.7 for older viewers.
//!
//! Each viewer gets two threads. One reads what the viewer sends: its pixel format and
//! encodings, its requests for updates, and its keys and pointer, which go to [`XClient`]. The
//! other answers the requests: it reads the screen, compares it in tiles with what it last
//! sent, and sends the tiles that changed, ZRLE when the viewer takes it and raw otherwise.
//! A request nothing has changed for waits for the screen to change.
//!
//! The cursor goes as x11vnc sends it. A viewer that takes cursor shapes (`Cursor`) is sent
//! the image to draw itself, again whenever it changes, and one that takes positions too
//! (`PointerPos`) is sent where the pointer is whenever it moves, so a pointer moved by a
//! program shows. A viewer that takes neither gets the cursor drawn into the screen.
//!
//! The reading half waits for data with `poll` and reads only what has come ([`Polled`]). A
//! port forwarded into a virtx guest is a vsock under its TCP socket, and a `recv` blocked on a
//! vsock holds the socket's lock, so a send from the other half would wait for the viewer to
//! say something.

use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use crate::{
    frame::Framebuffer,
    xclient::{Cursor, XClient},
    zrle::Zrle,
};

/// How often a waiting request looks at the screen again.
const POLL: Duration = Duration::from_millis(33);
/// The side of the tiles changes are found in.
const TILE: usize = 64;

const ENCODING_RAW: i32 = 0;
const ENCODING_ZRLE: i32 = 16;
const ENCODING_CURSOR: i32 = -239;
const ENCODING_POINTER_POS: i32 = -232;
const SECURITY_NONE: u8 = 1;
const SECURITY_VNC: u8 = 2;

pub struct Shared {
    pub framebuffer: PathBuf,
    pub password: Option<String>,
    pub name: String,
    pub xclient: Mutex<XClient>,
}

pub fn serve(listener: TcpListener, shared: Arc<Shared>) {
    for stream in listener.incoming().flatten() {
        let shared = shared.clone();
        std::thread::spawn(move || {
            let peer = stream
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_default();
            if let Err(e) = session(stream, &shared)
                && e.kind() != io::ErrorKind::UnexpectedEof
            {
                eprintln!("display: viewer {peer}: {e}");
            }
        });
    }
}

/// How the viewer wants pixels: RFC 6143 §7.4.
#[derive(Clone, Debug, PartialEq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub max: [u16; 3],
    pub shift: [u8; 3],
}

impl Default for PixelFormat {
    /// What this server offers: 32 bits a pixel, 8 of each colour, little-endian.
    fn default() -> Self {
        Self {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            true_colour: true,
            max: [255; 3],
            shift: [16, 8, 0],
        }
    }
}

impl PixelFormat {
    fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0; 16];
        b[0] = self.bits_per_pixel;
        b[1] = self.depth;
        b[2] = self.big_endian as u8;
        b[3] = self.true_colour as u8;
        for i in 0..3 {
            b[4 + 2 * i..6 + 2 * i].copy_from_slice(&self.max[i].to_be_bytes());
            b[10 + i] = self.shift[i];
        }
        b
    }

    fn from_bytes(b: &[u8; 16]) -> Self {
        let max = |i: usize| u16::from_be_bytes([b[4 + 2 * i], b[5 + 2 * i]]);
        Self {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_colour: b[3] != 0,
            max: [max(0), max(1), max(2)],
            shift: [b[10], b[11], b[12]],
        }
    }

    /// `0x00RRGGBB` as this format's pixel value.
    pub fn pixel(&self, rgb: u32) -> u32 {
        let channel = |i: usize, v: u32| {
            let max = u32::from(self.max[i]);
            ((v & 0xff) * max / 255) << self.shift[i]
        };
        channel(0, rgb >> 16) | channel(1, rgb >> 8) | channel(2, rgb)
    }

    fn bytes(&self) -> usize {
        usize::from(self.bits_per_pixel / 8)
    }

    fn write_pixel(&self, v: u32, out: &mut Vec<u8>) {
        let n = self.bytes();
        if self.big_endian {
            out.extend_from_slice(&v.to_be_bytes()[4 - n..]);
        } else {
            out.extend_from_slice(&v.to_le_bytes()[..n]);
        }
    }

    /// Where ZRLE's three-byte pixel sits in a 32-bit one, if it has one: every colour bit
    /// in the low three bytes (`Some(false)`) or in the high three (`Some(true)`).
    fn cpixel_high(&self) -> Option<bool> {
        if self.bits_per_pixel != 32 || self.depth > 24 || !self.true_colour {
            return None;
        }
        let used = (0..3).fold(0u32, |m, i| {
            m | u32::from(self.max[i])
                .checked_shl(u32::from(self.shift[i]))
                .unwrap_or(u32::MAX)
        });
        if used & 0xff00_0000 == 0 {
            Some(false)
        } else if used & 0xff == 0 {
            Some(true)
        } else {
            None
        }
    }

    pub fn cpixel_len(&self) -> usize {
        if self.cpixel_high().is_some() {
            3
        } else {
            self.bytes()
        }
    }

    pub fn write_cpixel(&self, v: u32, out: &mut Vec<u8>) {
        match self.cpixel_high() {
            None => self.write_pixel(v, out),
            Some(high) => {
                let v = if high { v >> 8 } else { v };
                if self.big_endian {
                    out.extend_from_slice(&v.to_be_bytes()[1..]);
                } else {
                    out.extend_from_slice(&v.to_le_bytes()[..3]);
                }
            }
        }
    }
}

/// What the reading half tells the answering half.
struct Viewer {
    format: PixelFormat,
    zrle: bool,
    /// Whether the viewer draws the cursor itself, from the shape it is sent.
    cursor_shape: bool,
    /// Whether the viewer moves its cursor to where it is told the pointer is.
    cursor_position: bool,
    /// Set by each `SetEncodings`, so the cursor is sent again in the way now asked for.
    encodings_changed: bool,
    /// A request waiting for an answer: whether it is incremental.
    request: Option<bool>,
    closed: bool,
}

fn session(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_nodelay(true)?;
    handshake(&mut stream, shared)?;

    let mut shared_flag = [0u8; 1];
    stream.read_exact(&mut shared_flag)?;
    let mut screen = Framebuffer::open(&shared.framebuffer)?;
    let (width, height) = (screen.width, screen.height);
    let format = PixelFormat::default();
    let mut init = Vec::new();
    init.extend_from_slice(&width.to_be_bytes());
    init.extend_from_slice(&height.to_be_bytes());
    init.extend_from_slice(&format.to_bytes());
    init.extend_from_slice(&(shared.name.len() as u32).to_be_bytes());
    init.extend_from_slice(shared.name.as_bytes());
    stream.write_all(&init)?;

    let viewer = Arc::new((
        Mutex::new(Viewer {
            format,
            zrle: false,
            cursor_shape: false,
            cursor_position: false,
            encodings_changed: false,
            request: None,
            closed: false,
        }),
        Condvar::new(),
    ));
    let reading = Polled(stream.try_clone()?);
    let viewer = &viewer;
    std::thread::scope(|scope| {
        // The reading half ends with the connection, and says so to the answering half.
        let reader = scope.spawn(move || {
            let result = read_messages(reading, viewer, &shared.xclient);
            let (state, wake) = &**viewer;
            state.lock().unwrap().closed = true;
            wake.notify_all();
            result
        });
        let answered = answer(&mut stream, &mut screen, viewer, &shared.xclient);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        let read = reader.join().unwrap();
        answered.and(read)
    })
}

/// Agree on a version and authenticate.
fn handshake(stream: &mut TcpStream, shared: &Shared) -> io::Result<()> {
    stream.write_all(b"RFB 003.008\n")?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version)?;
    let minor = match &version {
        b"RFB 003.003\n" => 3,
        b"RFB 003.007\n" => 7,
        // 3.8, and 3.889 from Apple's viewer, which speaks 3.8 to a server that offers it.
        _ if version.starts_with(b"RFB 003.") => 8,
        _ => return Err(io::Error::other("not a VNC viewer")),
    };
    let security = if shared.password.is_some() {
        SECURITY_VNC
    } else {
        SECURITY_NONE
    };
    if minor == 3 {
        stream.write_all(&u32::from(security).to_be_bytes())?;
    } else {
        stream.write_all(&[1, security])?;
        let mut chosen = [0u8; 1];
        stream.read_exact(&mut chosen)?;
        if chosen[0] != security {
            return Err(io::Error::other(
                "the viewer chose a security type not offered",
            ));
        }
    }

    let passed = match &shared.password {
        None => true,
        Some(password) => {
            let mut challenge = [0u8; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut challenge)?;
            stream.write_all(&challenge)?;
            let mut response = [0u8; 16];
            stream.read_exact(&mut response)?;
            response == vnc_auth(password, challenge)
        }
    };
    // 3.3 and 3.7 send no result for security type None.
    if minor == 8 || security == SECURITY_VNC {
        stream.write_all(&u32::from(!passed).to_be_bytes())?;
    }
    if !passed {
        if minor == 8 {
            let reason = b"wrong password";
            stream.write_all(&(reason.len() as u32).to_be_bytes())?;
            stream.write_all(reason)?;
        }
        return Err(io::Error::other("wrong password"));
    }
    Ok(())
}

/// The challenge DES-encrypted with the password, its first 8 bytes, each byte's bits reversed
/// as VNC has always done.
fn vnc_auth(password: &str, mut challenge: [u8; 16]) -> [u8; 16] {
    use des::cipher::{BlockEncrypt as _, KeyInit as _, generic_array::GenericArray};
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let cipher = des::Des::new(GenericArray::from_slice(&key));
    for block in challenge.as_chunks_mut::<8>().0 {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    challenge
}

/// A stream read only once `poll` says there is something to read.
struct Polled(TcpStream);

impl Read for Polled {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::fd::AsRawFd as _;
        let mut fd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`, for the length of the call.
        while unsafe { libc::poll(&mut fd, 1, -1) } == -1 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        self.0.read(buf)
    }
}

fn read_messages(
    mut stream: impl Read,
    viewer: &(Mutex<Viewer>, Condvar),
    xclient: &Mutex<XClient>,
) -> io::Result<()> {
    let (state, wake) = viewer;
    loop {
        let mut kind = [0u8; 1];
        stream.read_exact(&mut kind)?;
        match kind[0] {
            // SetPixelFormat
            0 => {
                let mut b = [0u8; 19];
                stream.read_exact(&mut b)?;
                let format = PixelFormat::from_bytes(b[3..].try_into().unwrap());
                if !format.true_colour || ![8, 16, 32].contains(&format.bits_per_pixel) {
                    return Err(io::Error::other("a viewer asked for colour-map pixels"));
                }
                let mut state = state.lock().unwrap();
                state.format = format;
                // Pixels sent before are in the old format: send everything again.
                state.request = Some(false);
                wake.notify_all();
            }
            // SetEncodings
            2 => {
                let mut b = [0u8; 3];
                stream.read_exact(&mut b)?;
                let n = usize::from(u16::from_be_bytes([b[1], b[2]]));
                let mut list = vec![0u8; n * 4];
                stream.read_exact(&mut list)?;
                let encodings: Vec<i32> = list
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&e| i32::from_be_bytes(e))
                    .collect();
                let mut state = state.lock().unwrap();
                state.zrle = encodings.contains(&ENCODING_ZRLE);
                state.cursor_shape = encodings.contains(&ENCODING_CURSOR);
                state.cursor_position = encodings.contains(&ENCODING_POINTER_POS);
                state.encodings_changed = true;
            }
            // FramebufferUpdateRequest: the area asked about is the whole screen, here.
            3 => {
                let mut b = [0u8; 9];
                stream.read_exact(&mut b)?;
                let incremental = b[0] != 0;
                let mut state = state.lock().unwrap();
                state.request = Some(incremental && state.request.unwrap_or(true));
                wake.notify_all();
            }
            // KeyEvent
            4 => {
                let mut b = [0u8; 7];
                stream.read_exact(&mut b)?;
                let keysym = u32::from_be_bytes(b[3..7].try_into().unwrap());
                xclient.lock().unwrap().key(keysym, b[0] != 0);
            }
            // PointerEvent
            5 => {
                let mut b = [0u8; 5];
                stream.read_exact(&mut b)?;
                let (x, y) = (
                    u16::from_be_bytes([b[1], b[2]]),
                    u16::from_be_bytes([b[3], b[4]]),
                );
                xclient.lock().unwrap().pointer(b[0], x, y);
            }
            // ClientCutText: the clipboard is not shared.
            6 => {
                let mut b = [0u8; 7];
                stream.read_exact(&mut b)?;
                let n = u32::from_be_bytes(b[3..7].try_into().unwrap());
                io::copy(&mut (&mut stream).take(u64::from(n)), &mut io::sink())?;
            }
            other => return Err(io::Error::other(format!("unknown message {other}"))),
        }
    }
}

fn answer(
    stream: &mut TcpStream,
    screen: &mut Framebuffer,
    viewer: &(Mutex<Viewer>, Condvar),
    xclient: &Mutex<XClient>,
) -> io::Result<()> {
    let (state, wake) = viewer;
    let (width, height) = (usize::from(screen.width), usize::from(screen.height));
    let mut sent: Vec<u32> = Vec::new();
    let mut now: Vec<u32> = Vec::new();
    let mut zrle = Zrle::new();
    let mut message = Vec::new();
    // The cursor's shape and position as last sent, for a viewer that draws it.
    let mut sent_serial: Option<u32> = None;
    let mut sent_position: Option<(i16, i16)> = None;

    loop {
        let (incremental, format, use_zrle, shape, position) = {
            let mut s = state.lock().unwrap();
            while s.request.is_none() && !s.closed {
                s = wake.wait(s).unwrap();
            }
            if s.closed {
                return Ok(());
            }
            if std::mem::take(&mut s.encodings_changed) {
                (sent_serial, sent_position) = (None, None);
                // A cursor drawn into what was sent, or one left out of it, is now wrong.
                sent.clear();
            }
            let request = s.request.unwrap();
            (
                request,
                s.format.clone(),
                s.zrle,
                s.cursor_shape,
                s.cursor_position,
            )
        };

        screen.read(&mut now)?;
        let cursor = xclient.lock().unwrap().cursor();
        if !shape && let Some(cursor) = &cursor {
            cursor.draw(&mut now, width, height);
        }
        let rects = if incremental && sent.len() == now.len() {
            changed(&sent, &now, width, height)
        } else {
            vec![(0, 0, width, height)]
        };
        let new_shape = cursor
            .as_ref()
            .filter(|c| shape && sent_serial != Some(c.serial));
        let new_position = cursor
            .as_ref()
            .map(|c| (c.x, c.y))
            .filter(|&p| shape && position && sent_position != Some(p));
        if rects.is_empty() && new_shape.is_none() && new_position.is_none() {
            // Nothing to say yet: look again, unless the request was replaced meanwhile.
            let s = state.lock().unwrap();
            let _ = wake.wait_timeout(s, POLL).unwrap();
            continue;
        }

        {
            let mut s = state.lock().unwrap();
            // A format change after this was read asks for a whole screen of its own.
            if s.format == format {
                s.request = None;
            }
        }

        let count = rects.len() + new_shape.is_some() as usize + new_position.is_some() as usize;
        message.clear();
        message.extend_from_slice(&[0, 0]);
        message.extend_from_slice(&(count as u16).to_be_bytes());
        if let Some(cursor) = new_shape {
            cursor_shape(cursor, &format, &mut message);
            sent_serial = Some(cursor.serial);
        }
        if let Some((x, y)) = new_position {
            rect_header(
                (x.max(0) as usize, y.max(0) as usize, 0, 0),
                ENCODING_POINTER_POS,
                &mut message,
            );
            sent_position = Some((x, y));
        }
        for &rect in &rects {
            let (x, y, w, h) = rect;
            if use_zrle {
                rect_header(rect, ENCODING_ZRLE, &mut message);
                zrle.encode(&now, width, rect, &format, &mut message);
            } else {
                rect_header(rect, ENCODING_RAW, &mut message);
                for row in y..y + h {
                    for &v in &now[row * width + x..row * width + x + w] {
                        format.write_pixel(format.pixel(v), &mut message);
                    }
                }
            }
        }
        stream.write_all(&message)?;
        std::mem::swap(&mut sent, &mut now);
    }
}

fn rect_header((x, y, w, h): (usize, usize, usize, usize), encoding: i32, out: &mut Vec<u8>) {
    for v in [x, y, w, h] {
        out.extend_from_slice(&(v as u16).to_be_bytes());
    }
    out.extend_from_slice(&encoding.to_be_bytes());
}

/// The `Cursor` pseudo-rectangle (RFC 6143 §7.8.2): the hot spot as its position, the image's
/// pixels in the viewer's format, then a bitmask of the ones that show, a row a whole byte.
fn cursor_shape(cursor: &Cursor, format: &PixelFormat, out: &mut Vec<u8>) {
    let (w, h) = (usize::from(cursor.width), usize::from(cursor.height));
    rect_header(
        (usize::from(cursor.hot.0), usize::from(cursor.hot.1), w, h),
        ENCODING_CURSOR,
        out,
    );
    let pixels: Vec<(u32, bool)> = cursor.pixels().collect();
    for &(rgb, _) in &pixels {
        format.write_pixel(format.pixel(rgb), out);
    }
    for row in pixels.chunks(w.max(1)) {
        for byte in row.chunks(8) {
            let bits = byte
                .iter()
                .enumerate()
                .fold(0u8, |b, (i, &(_, shown))| b | (u8::from(shown) << (7 - i)));
            out.push(bits);
        }
    }
}

/// The tiles that differ between `before` and `after`, those side by side in a row of tiles
/// joined into one rectangle.
fn changed(
    before: &[u32],
    after: &[u32],
    width: usize,
    height: usize,
) -> Vec<(usize, usize, usize, usize)> {
    let mut rects = Vec::new();
    for ty in (0..height).step_by(TILE) {
        let th = TILE.min(height - ty);
        let mut run: Option<(usize, usize)> = None;
        for tx in (0..width).step_by(TILE) {
            let tw = TILE.min(width - tx);
            let differs = (ty..ty + th).any(|row| {
                let at = row * width + tx;
                before[at..at + tw] != after[at..at + tw]
            });
            run = match (run, differs) {
                (Some((x, w)), true) => Some((x, w + tw)),
                (None, true) => Some((tx, tw)),
                (Some((x, w)), false) => {
                    rects.push((x, ty, w, th));
                    None
                }
                (None, false) => None,
            };
        }
        if let Some((x, w)) = run {
            rects.push((x, ty, w, th));
        }
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vnc_auth_is_des_under_the_password() {
        // An empty password is the all-zero key, and DES of a zero block under it is the
        // standard's test vector.
        let zero = [0x8c, 0xa6, 0x4d, 0xe9, 0xc1, 0xb1, 0x23, 0xa7];
        assert_eq!(vnc_auth("", [0; 16]), [zero, zero].concat()[..]);
    }

    #[test]
    fn a_three_byte_cpixel_is_the_low_bytes_of_the_default_format() {
        let format = PixelFormat::default();
        let mut out = Vec::new();
        format.write_cpixel(format.pixel(0x112233), &mut out);
        assert_eq!(out, [0x33, 0x22, 0x11]);
    }

    #[test]
    fn a_16_bit_format_scales_each_colour() {
        let format = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: true,
            true_colour: true,
            max: [31, 63, 31],
            shift: [11, 5, 0],
        };
        let mut out = Vec::new();
        format.write_pixel(format.pixel(0xffffff), &mut out);
        assert_eq!(out, [0xff, 0xff]);
        assert_eq!(format.cpixel_len(), 2);
    }

    #[test]
    fn a_cursor_shape_is_its_pixels_then_a_mask_a_row_a_byte() {
        let cursor = Cursor {
            x: 5,
            y: 5,
            hot: (1, 0),
            width: 9,
            height: 1,
            serial: 7,
            name: String::new(),
            argb: [vec![0xff112233; 8], vec![0]].concat(),
        };
        let mut out = Vec::new();
        cursor_shape(&cursor, &PixelFormat::default(), &mut out);
        assert_eq!(out[..12], [0, 1, 0, 0, 0, 9, 0, 1, 0xff, 0xff, 0xff, 0x11]);
        assert_eq!(out[12..16], [0x33, 0x22, 0x11, 0]);
        assert_eq!(out.len(), 12 + 9 * 4 + 2);
        assert_eq!(out[out.len() - 2..], [0xff, 0]);
    }

    #[test]
    fn changed_tiles_in_a_row_join() {
        let (w, h) = (200, 70);
        let before = vec![0; w * h];
        let mut after = before.clone();
        after[10] = 1; // tile (0, 0)
        after[70] = 1; // tile (64, 0)
        after[65 * w + 199] = 1; // tile (192, 64)
        assert_eq!(
            changed(&before, &after, w, h),
            [(0, 0, 128, 64), (192, 64, 8, 6)]
        );
    }
}
