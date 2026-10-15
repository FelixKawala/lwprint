//! Talking to the printer over Bluetooth serial (RFCOMM).

use anyhow::{bail, Context, Result};
use bluer::rfcomm::{Security, SecurityLevel, Socket, SocketAddr, Stream};
use bluer::Address;
use lw_core::protocol;
use lw_core::{pad_to_page, Bitmap, CutMode, JobOptions, Orientation, Status, StatusReader, LW_C410};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout, Instant};

/// Pause after each band of raster lines; Epson's library does this for the LW-C410.
const BAND_PAUSE: Duration = Duration::from_millis(500);

/// One piece of a print job and how long to wait after sending it.
pub struct Chunk {
    pub data: Vec<u8>,
    pub pause: Duration,
    /// Image data: once this is sent, the printer may print something.
    pub raster: bool,
}

/// A failed print, and how far it got.
#[derive(Debug)]
pub struct PrintFailure {
    /// False if the error happened before any image data was sent; such a print can
    /// safely be tried again without risking a duplicate label.
    pub started: bool,
    /// Indexes of the pages completely sent to the printer before the error.
    pub sent: Vec<usize>,
    pub error: anyhow::Error,
}

impl std::fmt::Display for PrintFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

impl std::error::Error for PrintFailure {}

/// Result of a job that ran to its end (possibly with pages skipped on request).
#[derive(Debug)]
pub struct PrintOutcome {
    /// Indexes of the pages sent and printed.
    #[allow(dead_code)]
    pub sent: Vec<usize>,
    /// A problem reported by the printer that did not prevent printing (e.g. low voltage).
    pub warning: Option<String>,
}

fn job_start(opts: &JobOptions) -> Vec<Chunk> {
    vec![
        Chunk { data: protocol::auto_status_request(), pause: Duration::ZERO, raster: false },
        Chunk { data: protocol::job_setup(opts), pause: Duration::ZERO, raster: false },
    ]
}

fn page_chunks(label: &Bitmap, orientation: Orientation) -> Vec<Chunk> {
    let mut chunks = vec![Chunk {
        data: protocol::page_setup(label.width() as u32, 0),
        pause: Duration::ZERO,
        raster: false,
    }];
    for band in protocol::raster_bands(label, orientation) {
        chunks.push(Chunk { data: band, pause: BAND_PAUSE, raster: true });
    }
    chunks.push(Chunk { data: protocol::page_end(), pause: Duration::ZERO, raster: false });
    chunks
}

/// One page of a job and the labels on it: (index into the label list, copies).
pub struct Page {
    pub bitmap: Bitmap,
    pub owners: Vec<(usize, usize)>,
}

/// The pages for printing several labels (each with a number of copies) as one job. With
/// cutting after each label, every copy is its own page (the printer cuts at page ends).
/// Otherwise everything goes onto one page as a strip with cut marks between labels, so
/// that only the strip, not every label, has to reach the printer's minimum page length.
pub fn batch_pages(labels: &[(Bitmap, usize)], cut: CutMode, cut_marks: bool) -> Vec<Page> {
    if cut == CutMode::Each || labels.iter().map(|(_, n)| *n).sum::<usize>() <= 1 {
        let mut pages = Vec::new();
        for (i, (label, copies)) in labels.iter().enumerate() {
            let page = pad_to_page(label, &LW_C410);
            for _ in 0..*copies {
                pages.push(Page { bitmap: page.clone(), owners: vec![(i, 1)] });
            }
        }
        return pages;
    }
    // Strip: all copies of all labels side by side, cut marks between neighbours.
    let height = labels.first().map(|(l, _)| l.height()).unwrap_or(0);
    let parts: Vec<&Bitmap> = labels.iter().flat_map(|(l, n)| std::iter::repeat_n(l, *n)).collect();
    let width: usize = parts.iter().map(|p| p.width()).sum();
    let mut joined = Bitmap::new(width, height);
    let mut x = 0;
    let mut boundaries = Vec::new();
    for p in &parts {
        if x > 0 {
            boundaries.push(x);
        }
        joined.blit(p, x as isize, 0);
        x += p.width();
    }
    if cut_marks {
        let tick = (height / 6).max(3);
        for b in boundaries {
            for xx in [b - 1, b] {
                for y in (0..tick).chain(height - tick..height) {
                    joined.set(xx, y, true);
                }
            }
        }
    }
    let owners = labels.iter().enumerate().map(|(i, (_, n))| (i, *n)).collect();
    vec![Page { bitmap: pad_to_page(&joined, &LW_C410), owners }]
}

/// Pages for `copies` of one label (see [`batch_pages`]).
pub fn job_pages(label: &Bitmap, copies: usize, cut: CutMode, cut_marks: bool) -> Vec<Bitmap> {
    batch_pages(&[(label.clone(), copies.max(1))], cut, cut_marks).into_iter().map(|p| p.bitmap).collect()
}

/// The complete byte sequence of a job with one page per label, in sending order.
/// All labels share one job, so the printer handles cutting between them itself.
pub fn job_chunks(pages: &[Bitmap], opts: &JobOptions, orientation: Orientation) -> Vec<Chunk> {
    let mut chunks = job_start(opts);
    for page in pages {
        chunks.extend(page_chunks(page, orientation));
    }
    chunks.push(Chunk { data: protocol::job_end(), pause: Duration::ZERO, raster: false });
    chunks
}

pub struct Printer {
    stream: Stream,
    reader: StatusReader,
    verbose: bool,
    started: Instant,
    /// Value of the socket's output-queue ioctl while nothing is queued.
    idle_queue: Option<u32>,
}

impl Printer {
    /// Open an encrypted RFCOMM connection (the printer drops unencrypted ones).
    pub async fn connect(addr: Address, channel: u8, verbose: bool) -> Result<Self> {
        let started = Instant::now();
        let socket = Socket::new().context("could not create a Bluetooth socket")?;
        socket.set_security(Security { level: SecurityLevel::Medium, key_size: 0 })?;
        let stream = timeout(Duration::from_secs(8), socket.connect(SocketAddr { addr, channel }))
            .await
            .context("timed out connecting to the printer (is it switched on and in range?)")?
            .with_context(|| format!("could not connect to {addr} on channel {channel}"))?;
        let idle_queue = stream.as_ref().output_buffer().ok();
        let p = Self { stream, reader: StatusReader::default(), verbose, started, idle_queue };
        p.log(&format!("connected to {addr} channel {channel} (idle send queue value {:?})", p.idle_queue));
        Ok(p)
    }

    /// Look up the channel if needed, connect, and check that the connection works by
    /// requesting the status. Tries a second time after a short pause, because the first
    /// connection to a printer that just woke up (or whose link is being torn down) can
    /// drop immediately. Returns the printer, the channel and its status.
    pub async fn open(addr: Address, channel: Option<u8>, verbose: bool) -> Result<(Self, u8, Status)> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=4 {
            if attempt > 1 {
                // Another connection attempt to the printer (from this program, the service,
                // or the kernel still paging a printer that was just switched on) makes new
                // attempts fail with "already in progress" or "busy"; give it time.
                let busy = last_err.as_ref().is_some_and(is_busy);
                let transient = last_err.as_ref().is_some_and(is_transient);
                if attempt > 2 && !busy && !transient {
                    break;
                }
                sleep(Duration::from_secs(if busy { 3 } else { 2 })).await;
            }
            let lookup = match channel {
                Some(_) => None,
                None => match crate::sdp::find_serial_channel(addr).await {
                    Ok(l) => Some(l),
                    Err(e) => {
                        last_err = Some(e.context("could not look up the printer's serial port channel"));
                        continue;
                    }
                },
            };
            let ch = channel.or(lookup.as_ref().map(|l| l.channel)).unwrap_or(6);
            match Self::connect(addr, ch, verbose).await {
                Ok(mut p) => {
                    drop(lookup);
                    match p.status().await {
                        Ok(st) => return Ok((p, ch, st)),
                        Err(e) => {
                            p.log(&format!("connection check failed: {e:#}"));
                            last_err = Some(e);
                        }
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
        let err = last_err.unwrap_or_else(|| anyhow::anyhow!("could not connect to the printer"));
        if is_busy(&err) {
            return Err(err.context(
                "another connection to the printer is in progress (is `lwprint serve` running? \
                 then print through it with --server or LWPRINT_SERVER)",
            ));
        }
        Err(err)
    }

    fn log(&self, msg: &str) {
        if self.verbose {
            eprintln!("[{:>6} ms] {msg}", self.started.elapsed().as_millis());
        }
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        let preview: Vec<String> = data.iter().take(12).map(|b| format!("{b:02x}")).collect();
        self.log(&format!("send {} bytes: {}{}", data.len(), preview.join(" "), if data.len() > 12 { " ..." } else { "" }));
        self.stream.write_all(data).await.context("sending to the printer failed")?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Wait up to `limit` for the next status report.
    async fn next_status(&mut self, limit: Duration) -> Result<Option<Status>> {
        let deadline = Instant::now() + limit;
        let mut buf = [0u8; 512];
        loop {
            if let Some(s) = self.reader.next_status() {
                let raw: Vec<String> = s.fields.iter().map(|(k, v)| format!("{k}:{v:02X}")).collect();
                self.log(&format!("status {}", raw.join(" ")));
                return Ok(Some(s));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match timeout(left, self.stream.read(&mut buf)).await {
                Err(_) => return Ok(None),
                Ok(Ok(0)) => bail!("the printer closed the connection"),
                Ok(Ok(n)) => {
                    self.log(&format!("received {n} bytes"));
                    self.reader.push(&buf[..n]);
                }
                Ok(Err(e)) => return Err(e).context("reading from the printer failed"),
            }
        }
    }

    /// Request and return the current status.
    pub async fn status(&mut self) -> Result<Status> {
        self.reader.clear();
        self.send(&protocol::status_request()).await?;
        self.next_status(Duration::from_secs(5))
            .await?
            .context("the printer did not answer the status request")
    }

    /// Print pages as one job and wait until the printer reports the job finished.
    /// `wanted(i)` is checked right before page `i` is sent; pages that are no longer
    /// wanted (their labels were stopped) are skipped, and the job still ends normally,
    /// so pages already sent print. `on_page_sent(i)` is called after page `i` was sent.
    pub async fn print_pages(
        &mut self,
        pages: &[Bitmap],
        opts: &JobOptions,
        orientation: Orientation,
        settle: Duration,
        wanted: &(dyn Fn(usize) -> bool + Send + Sync),
        on_page_sent: &mut (dyn FnMut(usize) + Send),
    ) -> Result<PrintOutcome, PrintFailure> {
        let mut started = false;
        let mut sent = Vec::new();
        if !(0..pages.len()).any(wanted) {
            return Ok(PrintOutcome { sent, warning: None });
        }
        let result =
            self.print_inner(pages, opts, orientation, settle, wanted, on_page_sent, &mut started, &mut sent).await;
        result.map_err(|error| PrintFailure { started, sent, error })
    }

    #[allow(clippy::too_many_arguments)]
    async fn print_inner(
        &mut self,
        pages: &[Bitmap],
        opts: &JobOptions,
        orientation: Orientation,
        settle: Duration,
        wanted: &(dyn Fn(usize) -> bool + Send + Sync),
        on_page_sent: &mut (dyn FnMut(usize) + Send),
        started: &mut bool,
        sent: &mut Vec<usize>,
    ) -> Result<PrintOutcome> {
        let st = self.status().await?;
        if !st.is_ready() {
            bail!("{}", describe_problem(&st));
        }
        if !settle.is_zero() {
            self.log(&format!("waiting {} ms before the job", settle.as_millis()));
            sleep(settle).await;
        }
        for chunk in job_start(opts) {
            self.send(&chunk.data).await?;
        }
        let mut total_mm = 0.0f32;
        for (i, page) in pages.iter().enumerate() {
            if !wanted(i) {
                self.log(&format!("skipping page {} (stopped)", i + 1));
                continue;
            }
            for chunk in page_chunks(page, orientation) {
                if chunk.raster {
                    *started = true;
                }
                self.send(&chunk.data).await?;
                if !chunk.pause.is_zero() {
                    sleep(chunk.pause).await;
                }
            }
            // Don't run far ahead of the printer, so that a stop request takes effect
            // after the labels already sent.
            self.wait_until_sent(Duration::from_secs(120)).await?;
            sent.push(i);
            total_mm += page.width() as f32 * 25.4 / 180.0;
            on_page_sent(i);
        }
        self.send(&protocol::job_end()).await?;

        // Sent data only reaches the kernel; wait until it has really gone out over
        // Bluetooth before doing anything else (closing early loses the rest of the image).
        self.wait_until_sent(Duration::from_secs(60)).await?;

        // Then poll the status until the job is done. The printer reports "printing"
        // (ST 02) while working and "print finished" (ST 05) at the end. It answers the
        // automatic-status command right away with a stale idle report, so idle only
        // counts after "printing" was seen or after enough time for the labels.
        let fallback = Duration::from_millis(4000 + (total_mm * 300.0) as u64);
        let give_up = Duration::from_secs(60) + fallback;
        let start = Instant::now();
        let mut seen_busy = false;
        // Problems reported mid-job never cut the wait short: we keep the connection
        // open and keep polling until the printer itself has finished or stopped.
        let mut warning: Option<String> = None;
        sleep(Duration::from_millis(300)).await;
        while start.elapsed() < give_up {
            self.reader.clear();
            self.send(&protocol::status_request()).await?;
            if let Some(s) = self.next_status(Duration::from_secs(5)).await? {
                if s.error() != 0 {
                    warning = Some(describe_problem(&s));
                }
                if s.is_busy() {
                    seen_busy = true;
                } else {
                    let finished = s.state() == protocol::STATE_PRINT_END
                        || (s.state() == protocol::STATE_IDLE && (seen_busy || start.elapsed() >= fallback));
                    let low_voltage_only = s.error() == protocol::ERROR_LOW_VOLTAGE;
                    if s.error() != 0 && !low_voltage_only {
                        // Not busy and a real error: the printer has stopped on its own.
                        bail!(
                            "{}{}",
                            describe_problem(&s),
                            if finished { " (labels were printed, but check them)" } else { "" }
                        );
                    }
                    if finished {
                        return Ok(PrintOutcome { sent: sent.clone(), warning });
                    }
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
        bail!("the printer did not finish within {}s; check the labels", give_up.as_secs())
    }

    /// Wait until the kernel has transmitted everything written to the socket.
    async fn wait_until_sent(&self, limit: Duration) -> Result<()> {
        let Some(idle) = self.idle_queue else {
            // Queue size unknown: allow roughly 2 kB/s plus a margin.
            sleep(Duration::from_secs(3)).await;
            return Ok(());
        };
        let start = Instant::now();
        loop {
            let now = self.stream.as_ref().output_buffer()?;
            if now == idle {
                self.log(&format!("all data transmitted after {} ms", start.elapsed().as_millis()));
                return Ok(());
            }
            if start.elapsed() > limit {
                bail!("data is not being transmitted to the printer");
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
}


fn os_errors(e: &anyhow::Error) -> Vec<i32> {
    e.chain().filter_map(|c| c.downcast_ref::<std::io::Error>()).filter_map(|io| io.raw_os_error()).collect()
}

/// True for errors meaning "someone else is connecting right now" (EBUSY, EALREADY, EINPROGRESS).
fn is_busy(e: &anyhow::Error) -> bool {
    os_errors(e).iter().any(|c| matches!(c, 16 | 114 | 115))
        || e.chain().any(|c| {
            let s = c.to_string();
            s.contains("already in progress") || s.contains("Device or resource busy")
        })
}

/// True for errors that usually go away after a moment: the link was being torn down or the
/// printer just woke up (ENOTCONN, ECONNRESET, ECONNABORTED, EHOSTDOWN, EHOSTUNREACH).
fn is_transient(e: &anyhow::Error) -> bool {
    os_errors(e).iter().any(|c| matches!(c, 103 | 104 | 107 | 112 | 113))
        || e.chain().any(|c| c.to_string().contains("not connected"))
}

/// Human-readable description of a status that prevents printing.
pub fn describe_problem(s: &Status) -> String {
    if s.error() != 0 {
        return format!("printer error {:02X}: {}", s.error(), protocol::error_name(s.error()));
    }
    if s.is_busy() {
        return format!("printer is busy ({})", protocol::state_name(s.state()));
    }
    "printer is ready".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_share_one_job() {
        let label = Bitmap::new(10, 72);
        let opts = JobOptions::default();
        let bytes: Vec<u8> = job_chunks(&[label.clone(), label.clone(), label], &opts, Orientation::Normal)
            .into_iter()
            .flat_map(|c| c.data)
            .collect();
        let count = |pat: &[u8]| bytes.windows(pat.len()).filter(|w| *w == pat).count();
        assert_eq!(count(&protocol::job_setup(&opts)), 1, "one job setup");
        assert_eq!(count(&[0x1B, 0x7B, 0x07, 0x4C]), 3, "one page setup per label");
        assert_eq!(count(&protocol::job_end()), 2, "reset at the start of the job setup and one job end");
        assert_eq!(bytes.iter().filter(|&&b| b == 0x0C).count() >= 3, true);
        assert!(bytes.ends_with(&[0x0C, 0x1B, 0x7B, 0x03, 0x40, 0x40, 0x7D]));
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn batches_pages() {
        let a = Bitmap::new(100, 72);
        let b = Bitmap::new(200, 72);
        let each = batch_pages(&[(a.clone(), 2), (b.clone(), 1)], CutMode::Each, true);
        assert_eq!(each.len(), 3);
        assert_eq!(each[2].owners, vec![(1, 1)]);
        let strip = batch_pages(&[(a, 2), (b, 1)], CutMode::End, true);
        assert_eq!(strip.len(), 1);
        assert_eq!(strip[0].bitmap.width(), 400);
        assert_eq!(strip[0].owners, vec![(0, 2), (1, 1)]);
    }
}
