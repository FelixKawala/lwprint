//! The print service behind `lwprint serve`: a job queue with one worker that owns
//! all printer access, plus a cached printer status. HTTP and MQTT both feed into it.

use crate::config::Config;
use crate::fonts::decode_data_url;
use crate::printer::{describe_problem, Printer};
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
const RETRY_PAUSE: Duration = Duration::from_secs(5);
/// Most labels (jobs) combined into one printer job.
const MAX_BATCH: usize = 25;

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
    /// Short marks between labels when they are not cut apart (strip printing).
    #[serde(default = "yes")]
    pub cut_marks: bool,
    /// Name of the person or automation printing (shown in the job list; not verified).
    #[serde(default)]
    pub requested_by: Option<String>,
}

fn yes() -> bool {
    true
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
    /// True while labels are held (the queue is paused).
    pub held: bool,
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
    /// Wakes a job that is waiting for the printer ("Check now", stop requests).
    wake: tokio::sync::Notify,
    /// While set, queued labels are kept instead of printed ("Hold labels").
    held: AtomicBool,
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
            wake: tokio::sync::Notify::new(),
            held: AtomicBool::new(false),
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

    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    /// Hold labels (pause the queue) or release them. Releasing prints everything that was
    /// collected, combined into as few printer jobs as the labels' settings allow.
    pub fn set_held(&self, hold: bool) {
        let was = self.held.swap(hold, Ordering::SeqCst);
        if was == hold {
            return;
        }
        if hold {
            log("holding labels");
        } else {
            let queued: Vec<u64> = self
                .jobs
                .lock()
                .unwrap()
                .iter()
                .filter(|j| j.info.state == JobState::Queued)
                .map(|j| j.info.id)
                .collect();
            log(format!("releasing {} held label job(s)", queued.len()));
            for id in queued {
                let _ = self.tx.send(WorkerMsg::Print(id));
            }
        }
        self.publish_status();
    }

    pub fn request_refresh(&self) {
        // A job waiting for the printer tries again right away; otherwise the worker
        // checks the printer.
        self.wake.notify_waiters();
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
        // Show what comes out of the printer: padded to the minimum page length, and all
        // copies on one strip when they are not cut apart.
        let label = self.render_with(&req.label, tape, Some(&notes))?;
        let pages = crate::printer::job_pages(&label, req.copies as usize, req.cut.mode(), req.cut_marks);
        let bitmap = pages.into_iter().next().unwrap_or(label);
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

    /// Validate and queue several labels at once; they print as one printer job when
    /// their settings allow it. Nothing is queued if any label is invalid.
    pub fn submit_all(&self, reqs: Vec<PrintRequest>, source: &str) -> Result<Vec<JobInfo>> {
        if reqs.is_empty() || reqs.len() > MAX_BATCH {
            return Err(anyhow!("send between 1 and {MAX_BATCH} labels at once"));
        }
        for (i, r) in reqs.iter().enumerate() {
            self.preview(r).with_context(|| format!("label {} cannot be rendered", i + 1))?;
        }
        reqs.into_iter().map(|r| self.submit(r, source)).collect()
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
        self.wake.notify_waiters();
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
            st.held = self.is_held();
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

    async fn connect(&self) -> Result<(Printer, Status)> {
        let cached = *self.channel.lock().unwrap();
        let (p, channel, st) = Printer::open(self.addr, cached, false).await?;
        *self.channel.lock().unwrap() = Some(channel);
        Ok((p, st))
    }

    async fn refresh(&self) {
        match self.connect().await {
            Ok((_, s)) => self.set_status(&s),
            Err(_) => self.set_offline("printer is switched off or out of range"),
        }
    }

    /// Connect and wait until the printer is ready, retrying until the deadline.
    /// Gives up early if all `ids` were stopped.
    async fn wait_for_printer(&self, ids: &[u64], deadline: Instant) -> Result<(Printer, Tape), WaitEnd> {
        let all_cancelled = || ids.iter().all(|id| self.is_cancelled(*id));
        loop {
            if all_cancelled() {
                return Err(WaitEnd::Cancelled);
            }
            let reason = match self.connect().await {
                Ok((p, s)) => {
                    self.set_status(&s);
                    match LW_C410.tape_by_code(s.tape_code()) {
                        Some(tape) if s.is_ready() => return Ok((p, tape)),
                        None => "no tape inserted".to_string(),
                        Some(_) => describe_problem(&s),
                    }
                }
                Err(_) => {
                    self.set_offline("printer is switched off or out of range");
                    "printer is switched off or out of range".to_string()
                }
            };
            if Instant::now() + RETRY_PAUSE > deadline {
                return Err(WaitEnd::GaveUp(format!("gave up waiting for the printer: {reason}")));
            }
            for id in ids.iter().filter(|id| !self.is_cancelled(**id)) {
                self.set_job_state(*id, JobState::Waiting, Some(format!("waiting: {reason}")));
            }
            // Wait a little, or less if someone presses "Check now" or stops a job.
            tokio::select! {
                _ = sleep(RETRY_PAUSE) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    fn is_queued(&self, id: u64) -> bool {
        self.jobs.lock().unwrap().iter().any(|j| j.info.id == id && j.info.state == JobState::Queued)
    }

    /// The job `first` plus all later queued jobs that can share a printer job with it
    /// (same density, cut mode, cut marks and orientation), in queue order.
    fn collect_batch(&self, first: u64) -> Vec<(u64, Arc<PrintRequest>)> {
        let jobs = self.jobs.lock().unwrap();
        let usable =
            |j: &JobEntry| j.info.state == JobState::Queued && !j.cancel.load(Ordering::SeqCst);
        let Some(head) = jobs.iter().find(|j| j.info.id == first && usable(j)) else {
            return Vec::new();
        };
        let key = |r: &PrintRequest| (r.density, r.cut.mode(), r.cut_marks, r.rotate);
        // Released holds can be large; MAX_BATCH keeps a single printer job manageable.
        let head_key = key(&head.request);
        jobs.iter()
            .filter(|j| j.info.id >= first && usable(j) && key(&j.request) == head_key)
            .take(MAX_BATCH)
            .map(|j| (j.info.id, j.request.clone()))
            .collect()
    }

    async fn process(&self, first: u64) {
        if !self.is_queued(first) || self.is_held() {
            // Already printed as part of an earlier batch, stopped, or held: held labels
            // stay queued and are sent to the worker again when they are released.
            return;
        }
        // Give a burst of jobs (e.g. from an automation) a moment to arrive, so that they
        // share one printer job and the blank lead is fed only once.
        let window = Duration::from_millis(self.config.printer.batch_window_ms);
        if !window.is_zero() {
            sleep(window).await;
        }
        if self.is_held() {
            return;
        }
        let batch = self.collect_batch(first);
        if batch.is_empty() {
            if self.is_cancelled(first) {
                self.finish_job(first, JobState::Cancelled, "nothing was printed".into());
            }
            return;
        }
        if batch.len() > 1 {
            let ids: Vec<String> = batch.iter().map(|(id, _)| id.to_string()).collect();
            log(format!("jobs {} share one printer job", ids.join(", ")));
        }
        self.run_batch(batch).await;
    }

    async fn run_batch(&self, batch: Vec<(u64, Arc<PrintRequest>)>) {
        let deadline = Instant::now() + Duration::from_secs(self.config.printer.retry_minutes * 60);
        let head = batch[0].1.clone();
        let opts = JobOptions { density: head.density, cut: head.cut.mode() };
        let orientation = if head.rotate { Orientation::Rotated } else { Orientation::Normal };
        // Copies printed per batch member; a Mutex because the progress callback must be Send.
        let done: Mutex<Vec<u32>> = Mutex::new(vec![0; batch.len()]);
        let mut finished = vec![false; batch.len()];
        let mut warnings: Vec<String> = Vec::new();
        let mut lengths = vec![0.0f32; batch.len()];
        let mut tape_mm = 0u8;
        let others = batch.len() - 1;

        'connect: loop {
            let active: Vec<usize> = (0..batch.len())
                .filter(|&i| !finished[i] && !self.is_cancelled(batch[i].0) && done.lock().unwrap()[i] < batch[i].1.copies)
                .collect();
            if active.is_empty() {
                break;
            }
            let ids: Vec<u64> = active.iter().map(|&i| batch[i].0).collect();
            let (mut p, tape) = match self.wait_for_printer(&ids, deadline).await {
                Ok(v) => v,
                Err(WaitEnd::Cancelled) => break,
                Err(WaitEnd::GaveUp(reason)) => {
                    for &i in &active {
                        let d = done.lock().unwrap()[i];
                        self.finish_job(batch[i].0, JobState::Failed, self.with_progress(reason.clone(), d, &batch[i].1));
                        finished[i] = true;
                    }
                    break;
                }
            };
            tape_mm = tape.mm;

            // Render every label for the inserted tape; jobs that can't print fail alone.
            let mut members: Vec<usize> = Vec::new();
            let mut labels: Vec<(Bitmap, usize)> = Vec::new();
            for &i in &active {
                let (id, req) = (batch[i].0, &batch[i].1);
                if let Some(mm) = req.tape_mm.filter(|mm| *mm != tape.mm) {
                    let msg = format!("label is for {mm} mm tape, but {} mm tape is inserted", tape.mm);
                    self.finish_job(id, JobState::Failed, msg);
                    finished[i] = true;
                    continue;
                }
                match self.render(&req.label, tape) {
                    Ok(label) => {
                        lengths[i] = label.width() as f32 * 25.4 / LW_C410.dpi as f32;
                        let remaining = (req.copies - done.lock().unwrap()[i]) as usize;
                        members.push(i);
                        labels.push((label, remaining));
                    }
                    Err(e) => {
                        self.finish_job(id, JobState::Failed, format!("{e:#}"));
                        finished[i] = true;
                    }
                }
            }
            if members.is_empty() {
                break;
            }
            let pages = crate::printer::batch_pages(&labels, opts.cut, head.cut_marks);
            let bitmaps: Vec<Bitmap> = pages.iter().map(|pg| pg.bitmap.clone()).collect();
            for &i in &members {
                self.set_job_state(batch[i].0, JobState::Printing, None);
            }

            let wanted = |pi: usize| pages[pi].owners.iter().any(|(m, _)| !self.is_cancelled(batch[members[*m]].0));
            let mut on_sent = |pi: usize| {
                for (m, copies) in &pages[pi].owners {
                    let i = members[*m];
                    let now = {
                        let mut d = done.lock().unwrap();
                        d[i] += *copies as u32;
                        d[i]
                    };
                    self.update_job(batch[i].0, |j| j.copies_done = now);
                }
                self.publish_status();
            };
            match p.print_pages(&bitmaps, &opts, orientation, Duration::ZERO, &wanted, &mut on_sent).await {
                Ok(out) => {
                    warnings.extend(out.warning);
                    if let Ok(s) = p.status().await {
                        self.set_status(&s);
                    }
                    break 'connect;
                }
                Err(f) if !f.started && Instant::now() < deadline => {
                    // Nothing was printed, so trying again cannot produce a duplicate.
                    log(format!("printer job not started ({f}); retrying"));
                    for &i in &members {
                        self.set_job_state(batch[i].0, JobState::Waiting, Some(format!("waiting: retrying after: {f}")));
                    }
                    sleep(Duration::from_secs(3)).await;
                    continue 'connect;
                }
                Err(f) => {
                    if let Ok(s) = p.status().await {
                        self.set_status(&s);
                    }
                    // The page being sent or printed when the error happened may be
                    // incomplete; its labels count as not printed, so a retry prints them.
                    let last_owners: Vec<usize> = f
                        .sent
                        .last()
                        .map(|pi| pages[*pi].owners.iter().map(|(m, _)| members[*m]).collect())
                        .unwrap_or_default();
                    for &i in &members {
                        let mut d = done.lock().unwrap()[i];
                        if last_owners.contains(&i) {
                            let on_last = f.sent.last().map(|pi| {
                                pages[*pi].owners.iter().find(|(m, _)| members[*m] == i).map(|(_, c)| *c).unwrap_or(0)
                            });
                            d = d.saturating_sub(on_last.unwrap_or(0) as u32);
                        }
                        self.update_job(batch[i].0, |j| j.copies_done = d);
                        let req = &batch[i].1;
                        let msg = if req.copies > 1 || others > 0 {
                            format!("{f} (check which labels printed)")
                        } else {
                            format!("{f}")
                        };
                        self.finish_job(batch[i].0, JobState::Failed, msg);
                        finished[i] = true;
                    }
                    return;
                }
            }
        }

        // Close every member that is still open: printed, or stopped part-way.
        let done = done.into_inner().unwrap();
        for (i, (id, req)) in batch.iter().enumerate() {
            if finished[i] {
                continue;
            }
            if done[i] >= req.copies {
                let mut msg = format!("{} × {:.0} mm on {} mm tape", req.copies, lengths[i], tape_mm);
                if others > 0 {
                    msg.push_str(&format!(", together with {others} other label{}", if others == 1 { "" } else { "s" }));
                }
                let mut w = warnings.clone();
                w.dedup();
                if !w.is_empty() {
                    msg.push_str(&format!(" (warning: {})", w.join("; ")));
                }
                self.finish_job(*id, JobState::Done, msg);
            } else {
                let msg = if done[i] == 0 {
                    "nothing was printed".to_string()
                } else {
                    format!("{} of {} printed", done[i], req.copies)
                };
                self.finish_job(*id, JobState::Cancelled, msg);
            }
        }
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
