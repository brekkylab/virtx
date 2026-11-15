//! The X server: Xvfb, built into this program, its screen in a file.

use std::{
    fs::File,
    io::Write as _,
    os::{
        fd::{AsRawFd as _, FromRawFd as _},
        unix::process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// How long the X server has to put up its socket and its screen.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// The directory a display's screen file and log are in.
fn dir(display: u16) -> PathBuf {
    PathBuf::from(format!("/tmp/display-{display}"))
}

/// The file Xvfb maps screen 0 of `display` to, an XWD image it draws into.
pub fn framebuffer_path(display: u16) -> PathBuf {
    dir(display).join("Xvfb_screen0")
}

/// The Xvfb this program runs, as `build.rs` fetched it: static, with its keymaps and fonts
/// inside it, so it needs nothing of the image.
const PROGRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/Xvfb"));

/// A running X server.
pub struct Server {
    child: Child,
    framebuffer: PathBuf,
}

impl Server {
    /// Start `:display` with one `width`×`height` screen at 24 bits, and wait for it to be up.
    pub fn start(display: u16, width: u16, height: u16) -> Result<Self, String> {
        let dir = dir(display);
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        let framebuffer = framebuffer_path(display);
        let _ = std::fs::remove_file(&framebuffer);
        let log_path = dir.join("Xvfb.log");
        let log = std::fs::File::create(&log_path)
            .map_err(|e| format!("creating {}: {e}", log_path.display()))?;

        // Run from a memfd: nothing is written where the image keeps its files, and a `noexec`
        // /tmp stops nothing. Closed on exec, which the kernel does only once it has the
        // program open.
        // SAFETY: a name with its NUL, and flags.
        let fd = unsafe { libc::memfd_create(c"Xvfb".as_ptr(), libc::MFD_CLOEXEC) };
        if fd == -1 {
            return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: a fd just opened, which nothing else owns.
        let mut program = unsafe { File::from_raw_fd(fd) };
        program
            .write_all(PROGRAM)
            .map_err(|e| format!("writing Xvfb: {e}"))?;

        let mut command = Command::new(format!("/proc/self/fd/{}", program.as_raw_fd()));
        command
            .arg0("Xvfb")
            .arg(format!(":{display}"))
            .args(["-screen", "0", &format!("{width}x{height}x24")])
            .arg("-fbdir")
            .arg(&dir)
            .args(["-nolisten", "tcp"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);
        crate::die_with_parent(&mut command);
        let child = command.spawn().map_err(|e| format!("starting Xvfb: {e}"))?;
        let mut server = Self { child, framebuffer };

        let socket = PathBuf::from(format!("/tmp/.X11-unix/X{display}"));
        let deadline = Instant::now() + START_TIMEOUT;
        while !(socket.exists() && server.framebuffer.exists()) {
            if let Ok(Some(status)) = server.child.try_wait() {
                return Err(format!(
                    "the X server ended as it started ({status}):\n{}",
                    tail(&log_path)
                ));
            }
            if Instant::now() > deadline {
                server.stop();
                return Err(format!(
                    "the X server was not up after {}s:\n{}",
                    START_TIMEOUT.as_secs(),
                    tail(&log_path)
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(server)
    }

    pub fn framebuffer(&self) -> &Path {
        &self.framebuffer
    }

    /// Wait for the X server to end, which is an error: nothing here ends it but [`stop`].
    ///
    /// [`stop`]: Self::stop
    pub fn wait(&mut self) -> Result<(), String> {
        let status = self.child.wait().map_err(|e| e.to_string())?;
        Err(format!("the X server ended ({status})"))
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The last lines of the X server's log, for an error that says why it did not start.
fn tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(15)..].join("\n")
}
