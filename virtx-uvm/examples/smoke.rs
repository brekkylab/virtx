//! Drive a `virtx-uvm` server end to end: a session on an image, a snapshot of it carried into a
//! second session, and a recipe built and booted.
//!
//! ```sh
//! cargo build --release -p virtx-uvm --example smoke
//! target/release/examples/smoke target/release/virtx-uvm [image]
//! ```
//!
//! Two more, each asked for, because each needs the network to install something:
//! `VIRTX_SMOKE_NET=1` publishes a guest port and fetches a page through it from this
//! machine, and `VIRTX_SMOKE_GPU=1` asks for a GPU and reads its name with `vulkaninfo` --
//! which takes a boot built with `gpu` (`cargo xtask build host`) and an image with Mesa's
//! venus driver, so `alpine:latest`.

use std::io::{Read, Write};

use virtx::console::ConsoleClient;
use virtx::image::Recipe;
use virtx::protocol::Port;

const TIMEOUT_MS: Option<u64> = Some(60_000);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let server = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("name the server to drive"))?;
    let image = args.next().unwrap_or_else(|| "alpine:3.21".to_string());

    eprintln!("== a session on {image}");
    let mut console = ConsoleClient::builder()
        .cmd(&[&server])
        .image(Recipe::new(&image))
        .build()
        .await?;
    let ran = console
        .exec(
            [
                "sh",
                "-c",
                "uname -sr && echo carried > /root/note && rm /etc/motd && cat /etc/alpine-release",
            ],
            TIMEOUT_MS,
        )
        .await?;
    show(&ran);
    anyhow::ensure!(ran.code == 0, "the first command exited {}", ran.code);

    let snapshot = console.snapshot().await?;
    eprintln!("== snapshot: {} bytes", snapshot.len());
    console.close().await?;

    eprintln!("== a second session, from the snapshot");
    let mut console = ConsoleClient::builder()
        .cmd(&[&server])
        .image(Recipe::new(&image))
        .snapshot(snapshot)
        .build()
        .await?;
    let ran = console
        .exec(
            [
                "sh",
                "-c",
                "cat /root/note && test ! -e /etc/motd && echo motd-gone",
            ],
            TIMEOUT_MS,
        )
        .await?;
    show(&ran);
    anyhow::ensure!(ran.code == 0, "the carried session exited {}", ran.code);
    console.close().await?;

    eprintln!("== a recipe, built and booted");
    let mut console = ConsoleClient::builder()
        .cmd(&[&server])
        .image(
            Recipe::new(&image)
                .step("echo built > /built")
                .step(virtx::image::Step::env("GREETING", "hello")),
        )
        .build()
        .await?;
    let ran = console
        .exec(["sh", "-c", "cat /built && echo $GREETING"], TIMEOUT_MS)
        .await?;
    show(&ran);
    anyhow::ensure!(ran.code == 0, "the built session exited {}", ran.code);
    console.close().await?;

    if std::env::var_os("VIRTX_SMOKE_NET").is_some_and(|on| on == "1") {
        eprintln!("== a published port, reached from this machine");
        const HOST: u16 = 18080;
        let mut console = ConsoleClient::builder()
            .cmd(&[&server])
            .image(Recipe::new(&image))
            .network(true)
            .ports([Port::new(HOST, 80)?])
            .build()
            .await?;
        let ran = console
            .exec(
                [
                    "sh",
                    "-c",
                    "apk add -q busybox-extras && mkdir -p /www && echo through > /www/index.html \
                     && (httpd -p 80 -h /www &) && sleep 1",
                ],
                TIMEOUT_MS,
            )
            .await?;
        show(&ran);
        anyhow::ensure!(ran.code == 0, "serving exited {}", ran.code);

        let mut stream = std::net::TcpStream::connect(("127.0.0.1", HOST))?;
        stream.write_all(b"GET / HTTP/1.0\r\n\r\n")?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer)?;
        let body = answer.split("\r\n\r\n").nth(1).unwrap_or_default();
        println!("{}", body.trim());
        anyhow::ensure!(body.trim() == "through", "the page came back as {answer:?}");
        console.close().await?;
    }

    if std::env::var_os("VIRTX_SMOKE_GPU").is_some_and(|on| on == "1") {
        eprintln!("== a GPU");
        let mut console = ConsoleClient::builder()
            .cmd(&[&server])
            .image(Recipe::new(&image))
            .network(true)
            .gpu(true)
            .build()
            .await?;
        let ran = console
            .exec(
                [
                    "sh",
                    "-c",
                    "apk add -q mesa-vulkan-virtio vulkan-loader vulkan-tools \
                     && vulkaninfo --summary 2>/dev/null | grep -E 'deviceName|driverName'",
                ],
                Some(300_000),
            )
            .await?;
        show(&ran);
        anyhow::ensure!(ran.code == 0, "vulkaninfo exited {}", ran.code);
        console.close().await?;
    }

    eprintln!("== all passed");
    Ok(())
}

fn show(ran: &virtx::protocol::ExecResp) {
    print!("{}", String::from_utf8_lossy(&ran.stdout));
    eprint!("{}", String::from_utf8_lossy(&ran.stderr));
    eprintln!("(exit {})", ran.code);
}
