//! Talking to the printer over Bluetooth serial (RFCOMM).

use anyhow::{bail, Context, Result};
use bluer::rfcomm::{Security, SecurityLevel, Socket, SocketAddr, Stream};
use bluer::Address;
use lw_core::protocol;
use lw_core::{Bitmap, JobOptions, Orientation, Status, StatusReader};
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

/// A failed print, and whether anything may have been printed.
#[derive(Debug)]
pub struct PrintFailure {
    /// False if the error happened before any image data was sent; such a print can
    /// safely be tried again without risking a duplicate label.
    pub started: bool,
    pub error: anyhow::Error,
}

impl std::fmt::Display for PrintFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

impl std::error::Error for PrintFailure {}

/// The complete byte sequence of one print job, in the order it is sent.
pub fn job_chunks(label: &Bitmap, opts: &JobOptions, orientation: Orientation) -> Vec<Chunk> {
    let none = Duration::ZERO;
    let mut chunks = vec![
        Chunk { data: protocol::auto_status_request(), pause: none, raster: false },
        Chunk { data: protocol::job_setup(opts), pause: none, raster: false },
        Chunk { data: protocol::page_setup(label.width() as u32, 0), pause: none, raster: false },
    ];
    for band in protocol::raster_bands(label, orientation) {
        chunks.push(Chunk { data: band, pause: BAND_PAUSE, raster: true });
    }
    chunks.push(Chunk { data: protocol::page_end(), pause: none, raster: false });
    chunks.push(Chunk { data: protocol::job_end(), pause: none, raster: false });
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
        let stream = timeout(Duration::from_secs(15), socket.connect(SocketAddr { addr, channel }))
            .await
            .context("timed out connecting to the printer (is it switched on and in range?)")?
            .with_context(|| format!("could not connect to {addr} on channel {channel}"))?;
        let idle_queue = stream.as_ref().output_buffer().ok();
        let p = Self { stream, reader: StatusReader::default(), verbose, started, idle_queue };
        p.log(&format!("connected to {addr} channel {channel} (idle send queue value {:?})", p.idle_queue));
        Ok(p)
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

    /// Print one label and wait until the printer reports the job finished.
    /// `settle` is an extra pause between the status check and the job.
    /// Returns a warning if the label printed but the printer reported a minor problem.
    pub async fn print(
        &mut self,
        label: &Bitmap,
        opts: &JobOptions,
        orientation: Orientation,
        settle: Duration,
    ) -> Result<Option<String>, PrintFailure> {
        let mut started = false;
        self.print_inner(label, opts, orientation, settle, &mut started)
            .await
            .map_err(|error| PrintFailure { started, error })
    }

    async fn print_inner(
        &mut self,
        label: &Bitmap,
        opts: &JobOptions,
        orientation: Orientation,
        settle: Duration,
        started: &mut bool,
    ) -> Result<Option<String>> {
        let st = self.status().await?;
        if !st.is_ready() {
            bail!("{}", describe_problem(&st));
        }
        if !settle.is_zero() {
            self.log(&format!("waiting {} ms before the job", settle.as_millis()));
            sleep(settle).await;
        }
        for chunk in job_chunks(label, opts, orientation) {
            if chunk.raster {
                *started = true;
            }
            self.send(&chunk.data).await?;
            if !chunk.pause.is_zero() {
                sleep(chunk.pause).await;
            }
        }

        // Sent data only reaches the kernel; wait until it has really gone out over
        // Bluetooth before doing anything else (closing early loses the rest of the image).
        self.wait_until_sent(Duration::from_secs(60)).await?;

        // Then poll the status until the label is done. The printer reports
        // "printing" (ST 02) while working and "print finished" (ST 05) at the end.
        // It answers the automatic-status command right away with a stale idle
        // report, so idle only counts after "printing" was seen or enough time passed.
        let label_mm = label.width() as f32 * 25.4 / 180.0;
        let fallback = Duration::from_millis(4000 + (label_mm * 300.0) as u64);
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
                            if finished { " (the label was printed, but check it)" } else { "" }
                        );
                    }
                    if finished {
                        return Ok(warning);
                    }
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
        bail!("the printer did not finish within {}s; check the label", give_up.as_secs())
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
