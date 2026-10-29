//! The nearest of virtx cloud's cells, as this machine reaches them: the control plane lists
//! the cells offered (`https://api.<domain>/v1/cells`), and each is timed at its gateway's
//! `/serving`, a few times over one connection, so that what is timed is the round trip and not
//! the handshake. A cell with no node up answers `503` there, and is passed by. The fastest of
//! the rest is the nearest.

use std::time::{Duration, Instant};

use serde::Deserialize;

/// How many times each cell is asked: the first opens the connection, and is not counted.
const ASKED: usize = 3;
/// How long a cell has to answer, each time.
const WAIT: Duration = Duration::from_secs(3);

/// A cell, as the control plane lists it.
#[derive(Clone, Debug, Deserialize)]
pub struct Cell {
    pub code: String,
    pub display_name: String,
    /// Where its gateway is reached: the endpoint a client connects to.
    pub api_url: String,
}

/// The cells offered under `domain`, nearest first, each with its round trip from here. A cell
/// that does not answer, or has no node up, is left out.
pub async fn cells_by_distance(domain: &str) -> anyhow::Result<Vec<(Cell, Duration)>> {
    let http = reqwest::Client::builder().timeout(WAIT).build()?;
    let listed = http
        .get(format!("https://api.{domain}/v1/cells"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let cells: Vec<Cell> = serde_json::from_slice(&listed)?;
    let timed = cells.into_iter().map(|cell| {
        let http = http.clone();
        async move {
            let url = format!("{}/serving", cell.api_url.trim_end_matches('/'));
            let mut fastest: Option<Duration> = None;
            for asked in 0..ASKED {
                let started = Instant::now();
                http.get(&url).send().await.ok()?.error_for_status().ok()?;
                let took = started.elapsed();
                if asked > 0 {
                    fastest = Some(fastest.map_or(took, |f| f.min(took)));
                }
            }
            Some((cell, fastest?))
        }
    });
    let mut timed: Vec<(Cell, Duration)> = futures_util::future::join_all(timed)
        .await
        .into_iter()
        .flatten()
        .collect();
    timed.sort_by_key(|(_, took)| *took);
    Ok(timed)
}

/// The endpoint of the nearest cell offered under `domain`.
pub async fn nearest_endpoint(domain: &str) -> anyhow::Result<String> {
    cells_by_distance(domain)
        .await?
        .into_iter()
        .next()
        .map(|(cell, _)| cell.api_url)
        .ok_or_else(|| anyhow::anyhow!("no cell under {domain} answered"))
}

#[cfg(feature = "ffi")]
pub(crate) mod ffi {
    use std::ffi::{CStr, c_char, c_int};

    use crate::ffi::{VirtxBytes, encode, write};

    /// Write the endpoint of the nearest cell of virtx cloud offered under `domain` -- the one
    /// this machine reaches fastest -- to `out`, as a JSON string; or, failing, the error, as
    /// every call's is. Blocks while the cells are timed: a few round trips to each, at once.
    ///
    /// # Safety
    ///
    /// `domain` is NUL-terminated UTF-8. `out` is writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_nearest_endpoint(
        domain: *const c_char,
        out: *mut VirtxBytes,
    ) -> c_int {
        let found = (|| {
            let domain = unsafe { CStr::from_ptr(domain) }
                .to_str()
                .map_err(|e| e.to_string())?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime
                .block_on(super::nearest_endpoint(domain))
                .map_err(|e| format!("{e:#}"))
        })();
        let (status, bytes) = encode(found);
        unsafe { write(out, bytes) };
        status
    }
}
