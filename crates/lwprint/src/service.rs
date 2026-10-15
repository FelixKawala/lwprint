//! The print service behind `lwprint serve`: a job queue with one worker that owns
//! all printer access, plus a cached printer status. HTTP and MQTT both feed into it.

use crate::config::Config;
use crate::fonts::decode_data_url;
use crate::printer::{describe_problem, Printer};
use crate::sdp;
use ab_glyph::FontArc;
use lw_core::IconFont;
use anyhow::{anyhow, Context, Result};
use bluer::Address;
use lw_core::protocol::state_name;
use lw_core::{render, Bitmap, CutMode, JobOptions, LabelSpec, Orientation, RenderContext, Status, Tape, LW_C410};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{sleep, sleep_until, Instant};

const KEEP_JOBS: usize = 100;
const RETRY_PAUSE: Duration = Duration::from_secs(15);

fn one() -> u32 {
    1
}

/// A print (or preview) request: a label plus print options, as one JSON object.
#[derive(Debug, Clone, Deserialize)]
pub struct PrintRequest {
    #[serde(flatten)]
    pub label: LabelSpec,
    #[serde(default = "one")]
    pub copies: u32,
    #[serde(default)]
    pub density: i8,
    /// "each" (default), "end" (one strip, cut after the last label) or "none";
    /// `true`/`false` mean "each"/"none".
    #[serde(default)]
    pub cut: CutField,
    #[serde(default)]
    pub rotate: bool,
    /// Tape width the label is meant for; printing fails if a different tape is inserted.
    /// For previews it picks the tape to render for.
    #[serde(default)]
    pub tape_mm: Option<u8>,
    /// Free-form ID echoed back in job results (useful with MQTT).
    #[serde(default)]
    pub request_id: Option<String>,
    /// Name of the person or automation printing (shown in the job list; not verified).
    #[serde(default)]
    pub requested_by: Option<String>,
}

/// The `cut` field: a cut mode, or a plain boolean for compatibility.
#[derive(Debug, Clone, Copy)]
pub enum CutField {
    Bool(bool),
    Mode(CutMode),
}

impl<'de> Deserialize<'de> for CutField {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(b) => Ok(CutField::Bool(b)),
            serde_json::Value::String(s) => match s.as_str() {
                "each" => Ok(CutField::Mode(CutMode::Each)),
                "end" => Ok(CutField::Mode(CutMode::End)),
                "none" => Ok(CutField::Mode(CutMode::None)),
                _ => Err(serde::de::Error::custom("cut must be \"each\", \"end\", \"none\", true or false")),
            },
            _ => Err(serde::de::Error::custom("cut must be \"each\", \"end\", \"none\", true or false")),
        }
    }
}

impl Default for CutField {
    fn default() -> Self {
        CutField::Mode(CutMode::Each)
    }
}

impl CutField {
    pub fn mode(self) -> CutMode {
        match self {
            CutField::Bool(true) => CutMode::Each,
            CutField::Bool(false) => CutMode::None,
            CutField::Mode(m) => m,
        }
    }
}

impl PrintRequest {
    /// Short description of the label for job lists, e.g. its first line of text.
    fn summary(&self) -> String {
        use lw_core::Element;
        let mut parts = Vec::new();
        for it in &self.label.elements {
            let s = match &it.element {
                Element::Text { text, .. } => text.lines().next().unwrap_or("").trim().to_string(),
                Element::Icon { name, .. } => format!("[{}]", name.trim_start_matches("mdi:")),
                Element::Qr { data, .. } => format!("[QR {data}]"),
                Element::Barcode { data, .. } => format!("[barcode {data}]"),
                Element::Image { .. } => "[image]".into(),
                Element::Space { .. } => continue,
            };
            if !s.is_empty() {
                parts.push(s);
            }
        }
        let mut s = parts.join(" ");
        if s.chars().count() > 60 {
            s = s.chars().take(57).collect::<String>() + "…";
        }
        s
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Waiting,
    Printing,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_active(self) -> bool {
        matches!(self, JobState::Queued | JobState::Waiting | JobState::Printing)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobInfo {
    pub id: u64,
    pub state: JobState,
    pub message: Option<String>,
    pub request_id: Option<String>,
    /// Name given by the sender (not verified).
    pub requested_by: Option<String>,
    /// Where the job came from, e.g. "web 192.168.1.20" or "mqtt".
    pub source: String,
    /// Short description of the label.
    pub summary: String,
    pub copies: u32,
    pub copies_done: u32,
    /// Set when stopping was requested while a label was printing.
    pub stopping: bool,
    /// The job this one retries, if any.
    pub retry_of: Option<u64>,
    pub created: u64,
    pub finished: Option<u64>,
}

struct JobEntry {
    info: JobInfo,
    request: Arc<PrintRequest>,
    cancel: Arc<AtomicBool>,
}

/// Printer status as published to clients.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PrinterState {
    pub printer_online: bool,
    pub tape_mm: Option<u8>,
    pub state: String,
    pub problem: Option<String>,
    pub queue: usize,
    pub last_result: Option<String>,
    pub updated: u64,
}

#[derive(Debug, Clone)]
pub enum Event {
    Status(PrinterState),
    JobFinished(JobInfo),
}

enum WorkerMsg {
    Print(u64),
    Refresh,
}

pub struct Service {
    pub config: Config,
    addr: Address,
    fonts: Vec<FontArc>,
    icons: IconFont,
    channel: Mutex<Option<u8>>,
    status: Mutex<PrinterState>,
    jobs: Mutex<VecDeque<JobEntry>>,
    next_id: Mutex<u64>,
    tx: mpsc::UnboundedSender<WorkerMsg>,
    events: broadcast::Sender<Event>,
}

/// Why waiting for the printer ended without a usable connection.
enum WaitEnd {
    Cancelled,
    GaveUp(String),
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Log a line with a UTC timestamp (the init system captures stderr).
pub fn log(msg: impl AsRef<str>) {
    let t = now();
    let (days, secs) = (t / 86400, t % 86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    eprintln!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} {}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        msg.as_ref()
    );
}

/// Images in service requests must be embedded as data URLs; file paths are refused
/// so that requests cannot read files from the server.
fn load_embedded_image(path: &str) -> Result<Vec<u8>, String> {
    decode_data_url(path).unwrap_or_else(|| {
        Err("the service only accepts embedded images (data: URLs)".into())
    })
}

impl Service {
    /// Create the service and start its worker task.
    pub fn start(config: Config, fonts: Vec<FontArc>, icons: IconFont) -> Result<Arc<Self>> {
        let addr: Address = config.printer.address.parse().context("invalid printer address in config")?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(64);
        let svc = Arc::new(Self {
            channel: Mutex::new(config.printer.channel),
            config,
            addr,
            fonts,
            icons,
            status: Mutex::new(PrinterState { state: "unknown".into(), ..Default::default() }),
            jobs: Mutex::new(VecDeque::new()),
            next_id: Mutex::new(1),
            tx,
            events,
        });
        tokio::spawn(worker(svc.clone(), rx));
        Ok(svc)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn status(&self) -> PrinterState {
        self.status.lock().unwrap().clone()
    }

    pub fn device_id(&self) -> String {
        format!("lwprint_{}", self.config.printer.address.replace(':', "").to_lowercase())
    }

    pub fn jobs(&self) -> Vec<JobInfo> {
        self.jobs.lock().unwrap().iter().rev().map(|j| j.info.clone()).collect()
    }

    pub fn job(&self, id: u64) -> Option<JobInfo> {
        self.jobs.lock().unwrap().iter().find(|j| j.info.id == id).map(|j| j.info.clone())
    }

    pub fn request_refresh(&self) {
        let _ = self.tx.send(WorkerMsg::Refresh);
    }

    /// Render a label without printing. Uses the requested tape, else the inserted one, else 12 mm.
    /// Render without printing; also returns hints such as hard-to-scan QR codes.
    pub fn preview(&self, req: &PrintRequest) -> Result<(Bitmap, Tape, Vec<String>)> {
        let mm = req.tape_mm.or(self.status().tape_mm).unwrap_or(12);
        let tape = LW_C410
            .tape_by_mm(mm)
            .ok_or_else(|| anyhow!("unsupported tape width {mm} mm (LW-C410: 4, 6, 9, 12, 18)"))?;
        let notes = std::cell::RefCell::new(Vec::new());
        let bitmap = self.render_with(&req.label, tape, Some(&notes))?;
        let mut notes = notes.into_inner();
        notes.dedup();
        Ok((bitmap, tape, notes))
    }

    fn render(&self, label: &LabelSpec, tape: Tape) -> Result<Bitmap> {
        self.render_with(label, tape, None)
    }

    fn render_with(
        &self,
        label: &LabelSpec,
        tape: Tape,
        notes: Option<&std::cell::RefCell<Vec<String>>>,
    ) -> Result<Bitmap> {
        let ctx = RenderContext {
            notes,
            model: LW_C410,
            tape,
            fonts: &self.fonts,
            icons: Some(&self.icons),
            load_image: &load_embedded_image,
        };
        Ok(render(label, &ctx)?)
    }

    /// Validate and queue a print request.
    pub fn submit(&self, req: PrintRequest, source: &str) -> Result<JobInfo> {
        self.enqueue(Arc::new(req), source, None)
    }

    fn enqueue(&self, req: Arc<PrintRequest>, source: &str, retry_of: Option<u64>) -> Result<JobInfo> {
        if req.copies == 0 || req.copies > 50 {
            return Err(anyhow!("copies must be between 1 and 50"));
        }
        if !(-5..=5).contains(&req.density) {
            return Err(anyhow!("density must be between -5 and 5"));
        }
        self.preview(&req).context("the label cannot be rendered")?;
        let id = {
            let mut n = self.next_id.lock().unwrap();
            let id = *n;
            *n += 1;
            id
        };
        let requested_by = req
            .requested_by
            .as_deref()
            .map(|s| s.trim().chars().take(40).collect::<String>())
            .filter(|s| !s.is_empty());
        let info = JobInfo {
            id,
            state: JobState::Queued,
            message: None,
            request_id: req.request_id.clone(),
            requested_by,
            source: source.to_string(),
            summary: req.summary(),
            copies: req.copies,
            copies_done: 0,
            stopping: false,
            retry_of,
            created: now(),
            finished: None,
        };
        {
            let mut jobs = self.jobs.lock().unwrap();
            jobs.push_back(JobEntry { info: info.clone(), request: req.clone(), cancel: Arc::new(AtomicBool::new(false)) });
            // Keep the history short, but never drop jobs that are still running.
            while jobs.len() > KEEP_JOBS && jobs.front().is_some_and(|j| !j.info.state.is_active()) {
                jobs.pop_front();
            }
        }
        let who = info.requested_by.as_deref().map(|n| format!("{n}, ")).unwrap_or_default();
        log(format!(
            "job {id}: queued by {who}{source}: \"{}\" ({} {})",
            info.summary,
            req.copies,
            if req.copies == 1 { "copy" } else { "copies" }
        ));
        self.tx.send(WorkerMsg::Print(id)).map_err(|_| anyhow!("print worker stopped"))?;
        self.publish_status();
        Ok(info)
    }

    /// Print a finished job again: the missing copies of a failed or stopped job,
    /// or all copies of a job that completed.
    pub fn retry(&self, id: u64, source: &str) -> Result<JobInfo> {
        let (request, info) = {
            let jobs = self.jobs.lock().unwrap();
            let j = jobs.iter().find(|j| j.info.id == id).ok_or_else(|| anyhow!("no such job"))?;
            (j.request.clone(), j.info.clone())
        };
        if info.state.is_active() {
            return Err(anyhow!("job {id} is still running"));
        }
        let remaining = info.copies - info.copies_done;
        let copies = if info.state == JobState::Done || remaining == 0 { info.copies } else { remaining };
        let mut req = (*request).clone();
        req.copies = copies;
        self.enqueue(Arc::new(req), source, Some(id))
    }

    /// Stop a job. Queued or waiting jobs stop right away; a printing job sends no further
    /// labels and ends normally, so labels already sent to the printer still print.
    pub fn cancel(&self, id: u64) -> Result<JobInfo> {
        let info = {
            let mut jobs = self.jobs.lock().unwrap();
            let j = jobs.iter_mut().find(|j| j.info.id == id).ok_or_else(|| anyhow!("no such job"))?;
            if !j.info.state.is_active() {
                return Err(anyhow!("job {id} has already finished"));
            }
            j.cancel.store(true, Ordering::SeqCst);
            if j.info.state == JobState::Printing {
                j.info.stopping = true;
                j.info.message = Some("stopping after the labels already sent to the printer".into());
            }
            j.info.clone()
        };
        log(format!("job {id}: stop requested"));
        if info.state != JobState::Printing {
            self.finish_job(id, JobState::Cancelled, "nothing was printed".into());
            return self.job(id).ok_or_else(|| anyhow!("no such job"));
        }
        self.publish_status();
        Ok(info)
    }

    /// Stop every queued, waiting or printing job.
    pub fn cancel_all(&self) -> usize {
        let ids: Vec<u64> =
            self.jobs.lock().unwrap().iter().filter(|j| j.info.state.is_active()).map(|j| j.info.id).collect();
        ids.iter().filter(|id| self.cancel(**id).is_ok()).count()
    }

    fn is_cancelled(&self, id: u64) -> bool {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.info.id == id)
            .is_none_or(|j| j.cancel.load(Ordering::SeqCst))
    }

    fn update_job(&self, id: u64, f: impl FnOnce(&mut JobInfo)) -> Option<JobInfo> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs.iter_mut().find(|j| j.info.id == id)?;
        f(&mut job.info);
        Some(job.info.clone())
    }

    fn set_job_state(&self, id: u64, state: JobState, message: Option<String>) {
        self.update_job(id, |j| {
            // A stop request keeps its message while the current label finishes.
            if !(j.stopping && state == JobState::Printing) {
                j.message = message;
            }
            j.state = state;
        });
        self.publish_status();
    }

    /// Mark a job as finished. Does nothing if it already finished (for example when a
    /// stop request and the worker both try to close it).
    fn finish_job(&self, id: u64, state: JobState, message: String) {
        let mut changed = false;
        let job = self.update_job(id, |j| {
            if j.state.is_active() {
                j.state = state;
                j.message = Some(message.clone());
                j.stopping = false;
                j.finished = Some(now());
                changed = true;
            }
        });
        if let (true, Some(job)) = (changed, job) {
            log(format!("job {id}: {state:?}: {message}"));
            self.status.lock().unwrap().last_result = Some(match state {
                JobState::Done => format!("printed: {message}"),
                JobState::Cancelled => format!("stopped: {message}"),
                _ => format!("failed: {message}"),
            });
            let _ = self.events.send(Event::JobFinished(job));
        }
        self.publish_status();
    }

    fn queue_len(&self) -> usize {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.info.state.is_active())
            .count()
    }

    fn publish_status(&self) {
        let queue = self.queue_len();
        let snapshot = {
            let mut st = self.status.lock().unwrap();
            st.queue = queue;
            st.clone()
        };
        let _ = self.events.send(Event::Status(snapshot));
    }

    fn set_status(&self, s: &Status) {
        {
            let mut st = self.status.lock().unwrap();
            st.printer_online = true;
            st.tape_mm = LW_C410.tape_by_code(s.tape_code()).map(|t| t.mm);
            st.state = state_name(s.state()).to_string();
            st.problem = if s.error() != 0 {
                Some(describe_problem(s))
            } else if st.tape_mm.is_none() {
                Some("no tape inserted".into())
            } else {
                None
            };
            st.updated = now();
        }
        self.publish_status();
    }

    fn set_offline(&self, reason: &str) {
        {
            let mut st = self.status.lock().unwrap();
            st.printer_online = false;
            st.state = "offline".into();
            st.problem = Some(reason.to_string());
            st.updated = now();
        }
        self.publish_status();
    }

    async fn connect(&self) -> Result<Printer> {
        let cached = *self.channel.lock().unwrap();
        let channel = match cached {
            Some(c) => c,
            None => {
                let c = sdp::find_serial_channel(self.addr).await?;
                *self.channel.lock().unwrap() = Some(c);
                c
            }
        };
        Printer::connect(self.addr, channel, false).await
    }

    async fn refresh(&self) {
        match self.connect().await {
            Ok(mut p) => match p.status().await {
                Ok(s) => self.set_status(&s),
                Err(e) => self.set_offline(&format!("{e:#}")),
            },
            Err(_) => self.set_offline("printer is switched off or out of range"),
        }
    }

    /// Connect and wait until the printer is ready, retrying until the deadline.
    async fn wait_for_printer(&self, id: u64, deadline: Instant) -> Result<(Printer, Tape), WaitEnd> {
        loop {
            if self.is_cancelled(id) {
                return Err(WaitEnd::Cancelled);
            }
            let reason = match self.connect().await {
                Ok(mut p) => match p.status().await {
                    Ok(s) => {
                        self.set_status(&s);
                        match LW_C410.tape_by_code(s.tape_code()) {
                            Some(tape) if s.is_ready() => return Ok((p, tape)),
                            None => "no tape inserted".to_string(),
                            Some(_) => describe_problem(&s),
                        }
                    }
                    Err(e) => format!("{e:#}"),
                },
                Err(_) => {
                    self.set_offline("printer is switched off or out of range");
                    "printer is switched off or out of range".to_string()
                }
            };
            if Instant::now() + RETRY_PAUSE > deadline {
                return Err(WaitEnd::GaveUp(format!("gave up waiting for the printer: {reason}")));
            }
            self.set_job_state(id, JobState::Waiting, Some(format!("waiting: {reason}")));
            // Sleep in small steps so that a stop request takes effect quickly.
            let until = Instant::now() + RETRY_PAUSE;
            while Instant::now() < until {
                if self.is_cancelled(id) {
                    return Err(WaitEnd::Cancelled);
                }
                sleep(Duration::from_millis(500)).await;
            }
        }
    }

    async fn process(&self, id: u64) {
        let Some(req) = self.jobs.lock().unwrap().iter().find(|j| j.info.id == id).map(|j| j.request.clone())
        else {
            return;
        };
        if self.is_cancelled(id) {
            return self.finish_job(id, JobState::Cancelled, "nothing was printed".into());
        }
        let deadline = Instant::now() + Duration::from_secs(self.config.printer.retry_minutes * 60);
        let opts = JobOptions { density: req.density, cut: req.cut.mode() };
        let orientation = if req.rotate { Orientation::Rotated } else { Orientation::Normal };
        let mut done = 0u32;
        let mut warnings: Vec<String> = Vec::new();
        let mut last_tape = None;
        let mut last_length = 0.0f32;

        'connect: while done < req.copies {
            let (mut p, tape) = match self.wait_for_printer(id, deadline).await {
                Ok(v) => v,
                Err(WaitEnd::Cancelled) => break 'connect,
                Err(WaitEnd::GaveUp(reason)) => return self.finish_job(id, JobState::Failed, self.with_progress(reason, done, &req)),
            };
            if let Some(mm) = req.tape_mm {
                if mm != tape.mm {
                    let msg = format!("label is for {mm} mm tape, but {} mm tape is inserted", tape.mm);
                    return self.finish_job(id, JobState::Failed, self.with_progress(msg, done, &req));
                }
            }
            // Render for the tape that is actually inserted (it may change between retries).
            let label = match self.render(&req.label, tape) {
                Ok(l) => l,
                Err(e) => return self.finish_job(id, JobState::Failed, format!("{e:#}")),
            };
            last_tape = Some(tape);
            last_length = label.width() as f32 * 25.4 / LW_C410.dpi as f32;
            self.set_job_state(id, JobState::Printing, None);

            if self.is_cancelled(id) {
                break 'connect;
            }
            // All remaining copies go into one printer job, so the printer cuts between
            // them without feeding blank tape for every label.
            let base = done;
            let pages: Vec<Bitmap> = (base..req.copies).map(|_| label.clone()).collect();
            let stop = || self.is_cancelled(id);
            let mut on_sent = |n: usize| {
                self.update_job(id, |j| j.copies_done = base + n as u32);
                self.publish_status();
            };
            match p.print_pages(&pages, &opts, orientation, Duration::ZERO, &stop, &mut on_sent).await {
                Ok(out) => {
                    done = base + out.pages_sent as u32;
                    warnings.extend(out.warning);
                    self.update_job(id, |j| j.copies_done = done);
                    if done < req.copies {
                        // Fewer labels than requested only happens when stopping was requested.
                        break 'connect;
                    }
                }
                Err(f) if !f.started && Instant::now() < deadline => {
                    // Nothing was printed, so trying again cannot produce a duplicate.
                    log(format!("job {id}: not started ({f}); retrying"));
                    self.set_job_state(id, JobState::Waiting, Some(format!("waiting: retrying after: {f}")));
                    sleep(Duration::from_secs(3)).await;
                    continue 'connect;
                }
                Err(f) => {
                    if let Ok(s) = p.status().await {
                        self.set_status(&s);
                    }
                    // The label being sent or printed when the error happened may be
                    // incomplete, so a retry prints it again.
                    let sent = base + f.pages_sent as u32;
                    self.update_job(id, |j| j.copies_done = sent.saturating_sub(1).max(base));
                    let msg = if req.copies > 1 {
                        format!("{f} ({sent} of {} labels were sent; check which ones printed)", req.copies)
                    } else {
                        format!("{f}")
                    };
                    return self.finish_job(id, JobState::Failed, msg);
                }
            }
            if let Ok(s) = p.status().await {
                self.set_status(&s);
            }
        }

        if done < req.copies {
            let msg = if done == 0 { "nothing was printed".to_string() } else { format!("{done} of {} printed", req.copies) };
            return self.finish_job(id, JobState::Cancelled, msg);
        }
        let tape_mm = last_tape.map(|t| t.mm).unwrap_or(0);
        let mut msg = format!("{} × {:.0} mm on {} mm tape", req.copies, last_length, tape_mm);
        warnings.dedup();
        if !warnings.is_empty() {
            msg.push_str(&format!(" (warning: {})", warnings.join("; ")));
        }
        self.finish_job(id, JobState::Done, msg);
    }

    fn with_progress(&self, msg: String, done: u32, req: &PrintRequest) -> String {
        if done > 0 { format!("{msg} ({done} of {} printed)", req.copies) } else { msg }
    }
}

async fn worker(svc: Arc<Service>, mut rx: mpsc::UnboundedReceiver<WorkerMsg>) {
    let interval = Duration::from_secs(svc.config.printer.status_interval_secs);
    // Check the printer once shortly after start, then at the configured interval.
    let mut next_poll = Instant::now() + Duration::from_secs(2);
    loop {
        let msg = if interval.is_zero() && svc.status().updated != 0 {
            rx.recv().await
        } else {
            tokio::select! {
                m = rx.recv() => m,
                _ = sleep_until(next_poll) => Some(WorkerMsg::Refresh),
            }
        };
        match msg {
            None => break,
            Some(WorkerMsg::Print(id)) => svc.process(id).await,
            Some(WorkerMsg::Refresh) => svc.refresh().await,
        }
        next_poll = Instant::now() + if interval.is_zero() { Duration::from_secs(2) } else { interval };
    }
}
