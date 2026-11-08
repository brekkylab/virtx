//! `display` — a screen for GUI programs, watched and driven over VNC.
//!
//! ```text
//! display run -- openttd
//! display shot /artifacts/shots/turn-3.png
//! display cursor
//! ```
//!
//! `run` starts an X server with its screen in memory ([`xvfb`]), serves that screen over VNC
//! ([`rfb`]), and runs the command given with `DISPLAY` pointing at it. The X server is
//! `/abin/libexec/display/Xvfb`, beside this program, so an image needs the program it shows
//! and nothing of X's server side.
//!
//! The screen is a file the X server keeps mapped ([`frame`]): what VNC sends and what `shot`
//! saves are read from it rather than asked of the X server. A viewer's keys and pointer, and
//! the cursor VNC and `cursor` report, go through a connection to the X server ([`xclient`]).

mod frame;
mod rfb;
mod xclient;
mod xvfb;
mod zrle;

use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, TcpListener},
    path::PathBuf,
    process::{Command, ExitCode},
    sync::Arc,
};

use clap::Parser as _;

const NAME: &str = env!("CARGO_BIN_NAME");

/// Run GUI programs on a screen in memory, watched and driven over VNC
///
/// A screen is an X display, `:99` unless `--display` says otherwise. `run` keeps one up for
/// as long as it runs; `shot` reads it from any other command on the same machine meanwhile.
///
/// The X server comes with this program. An image needs the GUI program and the X libraries it
/// links, and no X server, VNC server or screenshot tool of its own.
#[derive(Debug, clap::Parser)]
#[command(arg_required_else_help = true)]
enum Cli {
    /// Start a screen, serve it over VNC, and run a command on it
    ///
    /// The command runs with `DISPLAY` set to the screen. `ready` is printed on a line of its
    /// own once the screen and VNC are up. With a command, this exits when the command does,
    /// with its status, and takes the screen down with it; without one, the screen stays up
    /// until this is stopped.
    ///
    /// A viewer sees the screen and types and clicks into it as a keyboard and mouse would. The
    /// cursor shows where the pointer is, a program moving it included: drawn by the viewer
    /// when it can, into the screen it is sent otherwise. The clipboard is not shared. Programs
    /// that need OpenGL from the X server (GLX) do not start on this screen.
    #[command(after_long_help = "Examples:\n  \
        display run -- xterm\n  \
        display run --size 1920x1080 --password secret -- firefox https://example.com\n  \
        display run --display 100 --port 5901 -- xclock")]
    Run(Run),

    /// Save what a screen shows now as a PNG
    ///
    /// Read from the screen `display run` keeps, from any command on the same machine while it
    /// runs. The cursor is not in it: `display cursor` says where it is. Prints the path
    /// written.
    #[command(after_long_help = "Example:\n  display shot /tmp/screen.png")]
    Shot(Shot),

    /// Print where the pointer is on a screen, and the cursor's name
    ///
    /// One line: the pointer's x and y in pixels from the screen's top left corner, then the
    /// name of the cursor shown there when it has one. The name is what the program under the
    /// pointer chose to show, and so says what is there: `xterm` over text, `hand2` over a
    /// link, `left_ptr` over most else.
    ///
    /// A cursor has a name when its program loaded it from a cursor theme, through libXcursor.
    /// GTK and Qt programs bring both; for another X program, an image without libXcursor and
    /// a cursor theme gives positions only.
    #[command(after_long_help = "Example:\n  $ display cursor\n  640 360 xterm")]
    Cursor(CursorArgs),
}

#[derive(Debug, clap::Args)]
struct Run {
    /// The screen's size in pixels, at 24-bit colour
    #[arg(long, value_name = "WxH", default_value = "1280x720", value_parser = size)]
    size: (u16, u16),

    /// The screen's X display number. The screen is `:N`, and `shot --display N` reads it; two
    /// screens at once need two numbers and two ports
    #[arg(long, value_name = "N", default_value_t = 99)]
    display: u16,

    /// The TCP port VNC listens at. To watch from outside a sandbox, open this port when the
    /// sandbox is made and forward it
    #[arg(long, value_name = "PORT", default_value_t = 5900)]
    port: u16,

    /// The address VNC listens at. A port forwarded into the sandbox reaches 127.0.0.1
    #[arg(long, value_name = "ADDR", default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    listen: IpAddr,

    /// The password a viewer has to give; VNC counts its first 8 characters. Without one, any
    /// viewer that reaches the port is let in
    #[arg(
        long,
        value_name = "PASSWORD",
        env = "DISPLAY_VNC_PASSWORD",
        hide_env_values = true
    )]
    password: Option<String>,

    /// The program to run on the screen and its arguments, after `--`
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<OsString>,
}

#[derive(Debug, clap::Args)]
struct Shot {
    /// Where to write the PNG. Its directory has to be there already
    #[arg(value_name = "FILE")]
    file: PathBuf,

    /// The X display number of the screen to save
    #[arg(long, value_name = "N", default_value_t = 99)]
    display: u16,
}

#[derive(Debug, clap::Args)]
struct CursorArgs {
    /// The X display number of the screen
    #[arg(long, value_name = "N", default_value_t = 99)]
    display: u16,
}

fn size(s: &str) -> Result<(u16, u16), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or("expected WIDTHxHEIGHT, as 1280x720")?;
    let w: u16 = w.parse().map_err(|e| format!("width: {e}"))?;
    let h: u16 = h.parse().map_err(|e| format!("height: {e}"))?;
    if w == 0 || h == 0 {
        return Err("a screen has no zero side".into());
    }
    Ok((w, h))
}

fn main() -> ExitCode {
    let result = match Cli::parse() {
        Cli::Run(args) => run(args),
        Cli::Shot(args) => shot(args).map(|()| ExitCode::SUCCESS),
        Cli::Cursor(args) => cursor(args).map(|()| ExitCode::SUCCESS),
    };
    result.unwrap_or_else(|e| {
        eprintln!("{NAME}: {e}");
        ExitCode::FAILURE
    })
}

fn run(args: Run) -> Result<ExitCode, String> {
    let (width, height) = args.size;
    let mut server = xvfb::Server::start(args.display, width, height)?;
    let xclient = xclient::XClient::connect(args.display)?;
    let listener = TcpListener::bind((args.listen, args.port))
        .map_err(|e| format!("listening at {}:{}: {e}", args.listen, args.port))?;

    let shared = Arc::new(rfb::Shared {
        framebuffer: server.framebuffer().to_owned(),
        password: args.password,
        name: format!("display :{}", args.display),
        xclient: std::sync::Mutex::new(xclient),
    });
    std::thread::spawn(move || rfb::serve(listener, shared));

    eprintln!(
        "{NAME}: :{} at {width}x{height}, VNC at {}:{}",
        args.display, args.listen, args.port
    );
    println!("ready");

    let Some((program, rest)) = args.command.split_first() else {
        return server.wait().map(|()| ExitCode::FAILURE);
    };
    let mut command = Command::new(program);
    command
        .args(rest)
        .env("DISPLAY", format!(":{}", args.display));
    die_with_parent(&mut command);
    let status = command
        .status()
        .map_err(|e| format!("running {}: {e}", program.to_string_lossy()))?;
    server.stop();
    Ok(match status.code() {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::FAILURE,
    })
}

fn shot(args: Shot) -> Result<(), String> {
    let framebuffer = xvfb::framebuffer_path(args.display);
    let mut screen = frame::Framebuffer::open(&framebuffer).map_err(|e| {
        format!(
            "no screen :{} to take ({}: {e}); is `{NAME} run` running?",
            args.display,
            framebuffer.display()
        )
    })?;
    let mut pixels = Vec::new();
    screen
        .read(&mut pixels)
        .map_err(|e| format!("reading the screen: {e}"))?;
    frame::write_png(&screen, &pixels, &args.file)
        .map_err(|e| format!("writing {}: {e}", args.file.display()))?;
    println!("{}", args.file.display());
    Ok(())
}

fn cursor(args: CursorArgs) -> Result<(), String> {
    let cursor = xclient::XClient::connect(args.display)
        .map_err(|e| format!("{e}; is `{NAME} run` running?"))?
        .cursor()
        .ok_or("the X server would not say where the cursor is")?;
    if cursor.name.is_empty() {
        println!("{} {}", cursor.x, cursor.y);
    } else {
        println!("{} {} {}", cursor.x, cursor.y, cursor.name);
    }
    Ok(())
}

/// Have `command`'s process sent SIGTERM when this one ends, however it ends.
pub fn die_with_parent(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: `prctl` is async-signal-safe and touches nothing of the parent's.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}
