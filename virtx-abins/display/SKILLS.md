---
name: display
description: Run GUI programs on a screen in memory, watched and driven over VNC
---

Run GUI programs on a screen in memory, watched and driven over VNC

A screen is an X display, `:99` unless `--display` says otherwise. `run` keeps one up for as long as it runs; `shot` reads it from any other command on the same machine meanwhile.

The X server comes with this program. An image needs the GUI program and the X libraries it links, and no X server, VNC server or screenshot tool of its own.

Usage: display <COMMAND>

Commands:
  run     Start a screen, serve it over VNC, and run a command on it
  shot    Save what a screen shows now as a PNG
  cursor  Print where the pointer is on a screen, and the cursor's name
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

## display run

Start a screen, serve it over VNC, and run a command on it

The command runs with `DISPLAY` set to the screen. `ready` is printed on a line of its own once the screen and VNC are up. With a command, this exits when the command does, with its status, and takes the screen down with it; without one, the screen stays up until this is stopped.

A viewer sees the screen and types and clicks into it as a keyboard and mouse would. The cursor shows where the pointer is, a program moving it included: drawn by the viewer when it can, into the screen it is sent otherwise. The clipboard is not shared. Programs that need OpenGL from the X server (GLX) do not start on this screen.

Usage: display run [OPTIONS] [-- <COMMAND>...]

Arguments:
  [COMMAND]...
          The program to run on the screen and its arguments, after `--`

Options:
      --size <WxH>
          The screen's size in pixels, at 24-bit colour
          
          [default: 1280x720]

      --display <N>
          The screen's X display number. The screen is `:N`, and `shot --display N` reads it; two screens at once need two numbers and two ports
          
          [default: 99]

      --port <PORT>
          The TCP port VNC listens at. To watch from outside a sandbox, open this port when the sandbox is made and forward it
          
          [default: 5900]

      --listen <ADDR>
          The address VNC listens at. A port forwarded into the sandbox reaches 127.0.0.1
          
          [default: 127.0.0.1]

      --password <PASSWORD>
          The password a viewer has to give; VNC counts its first 8 characters. Without one, any viewer that reaches the port is let in
          
          [env: DISPLAY_VNC_PASSWORD]

  -h, --help
          Print help (see a summary with '-h')

Examples:
  display run -- xterm
  display run --size 1920x1080 --password secret -- firefox https://example.com
  display run --display 100 --port 5901 -- xclock

## display shot

Save what a screen shows now as a PNG

Read from the screen `display run` keeps, from any command on the same machine while it runs. The cursor is not in it: `display cursor` says where it is. Prints the path written.

Usage: display shot [OPTIONS] <FILE>

Arguments:
  <FILE>
          Where to write the PNG. Its directory has to be there already

Options:
      --display <N>
          The X display number of the screen to save
          
          [default: 99]

  -h, --help
          Print help (see a summary with '-h')

Example:
  display shot /tmp/screen.png

## display cursor

Print where the pointer is on a screen, and the cursor's name

One line: the pointer's x and y in pixels from the screen's top left corner, then the name of the cursor shown there when it has one. The name is what the program under the pointer chose to show, and so says what is there: `xterm` over text, `hand2` over a link, `left_ptr` over most else.

A cursor has a name when its program loaded it from a cursor theme, through libXcursor. GTK and Qt programs bring both; for another X program, an image without libXcursor and a cursor theme gives positions only.

Usage: display cursor [OPTIONS]

Options:
      --display <N>
          The X display number of the screen
          
          [default: 99]

  -h, --help
          Print help (see a summary with '-h')

Example:
  $ display cursor
  640 360 xterm
