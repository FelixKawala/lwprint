//! `lwprint`: print labels on an Epson LabelWorks LW-C410 over Bluetooth.

mod client;
mod config;
mod fonts;
mod http;
mod icons;
mod mqtt;
mod printer;
mod sdp;
mod service;

use anyhow::{anyhow, bail, Context, Result};
use fonts::load_font;
use bluer::Address;
use clap::{Args, Parser, Subcommand, ValueEnum};
use lw_core::{render, Element, JobOptions, LabelSpec, Orientation, RenderContext, Symbology, LW_C410};
use printer::Printer;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(version, about = "Print labels on an Epson LabelWorks LW-C410 over Bluetooth")]
struct Cli {
    /// Printer Bluetooth address, e.g. 44:B7:D0:64:FC:0E
    #[arg(long, short, env = "LWPRINT_DEVICE", global = true)]
    device: Option<String>,
    /// RFCOMM channel of the printer's serial port (looked up automatically if omitted)
    #[arg(long, env = "LWPRINT_CHANNEL", global = true)]
    channel: Option<u8>,
    /// Log every step with timestamps to stderr
    #[arg(long, short, global = true)]
    verbose: bool,
    /// Print through a running `lwprint serve` (e.g. http://labelprinter.lan:8080) instead
    /// of connecting to the printer directly
    #[arg(long, env = "LWPRINT_SERVER", global = true)]
    server: Option<String>,
    /// Access token for the service, if it requires one
    #[arg(long, env = "LWPRINT_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the printer's status and inserted tape
    Status,
    /// Print a label (or save a preview with --preview)
    Print(PrintArgs),
    /// Search the bundled icons (Material Design Icons) or emoji
    Icons {
        /// Search words, e.g. "screw" or "light bulb"
        query: Vec<String>,
        /// Search emoji instead of icons
        #[arg(long)]
        emoji: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Run the print service (HTTP API, web editor, optional MQTT)
    Serve {
        /// Configuration file
        #[arg(long, default_value = "/etc/lwprint.toml")]
        config: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CutArg {
    Each,
    End,
    None,
}

#[derive(Clone, Copy, ValueEnum)]
enum BorderArg {
    None,
    Line,
    Double,
    Rounded,
    Dashed,
}

#[derive(Clone, Copy, ValueEnum)]
enum BarcodeType {
    Code128,
    Ean13,
    Code39,
}

#[derive(Args)]
struct PrintArgs {
    /// Line of text; repeat for more lines
    #[arg(long, short)]
    text: Vec<String>,
    /// QR code content
    #[arg(long)]
    qr: Option<String>,
    /// Use equal QR squares instead of filling the tape height (fill is the default)
    #[arg(long)]
    qr_equal: bool,
    /// Icon name from Material Design Icons (see `lwprint icons <search>`); repeat for more
    #[arg(long)]
    icon: Vec<String>,
    /// Barcode content
    #[arg(long)]
    barcode: Option<String>,
    #[arg(long, value_enum, default_value = "code128")]
    barcode_type: BarcodeType,
    /// Image file (PNG or JPEG), scaled to the tape height
    #[arg(long)]
    image: Option<PathBuf>,
    /// Dither the image instead of a hard black/white threshold (better for photos)
    #[arg(long)]
    dither: bool,
    /// Label description as JSON ("-" reads stdin); replaces the content options above
    #[arg(long)]
    spec: Option<PathBuf>,
    /// Save the label as a PNG instead of printing it
    #[arg(long)]
    preview: Option<PathBuf>,
    /// Save the exact bytes of the print job to a file instead of printing (for debugging)
    #[arg(long)]
    dump: Option<PathBuf>,
    /// Extra pause in milliseconds between the status check and the print job
    #[arg(long, default_value_t = 0)]
    settle_ms: u64,
    /// Tape width in mm; detected automatically when printing (default 12 for previews)
    #[arg(long)]
    tape: Option<u8>,
    /// Font family name or path to a .ttf/.otf file
    #[arg(long)]
    font: Option<String>,
    /// Text line height in mm (default: lines share the tape height)
    #[arg(long)]
    text_size: Option<f32>,
    /// Print density from -5 (light) to 5 (dark)
    #[arg(long, default_value_t = 0, allow_hyphen_values = true,
          value_parser = clap::value_parser!(i8).range(-5..=5))]
    density: i8,
    /// When to cut: after each label, only after the last one (one strip), or never
    #[arg(long, value_enum, default_value = "each")]
    cut: CutArg,
    /// Same as --cut none
    #[arg(long, hide = true)]
    no_cut: bool,
    /// Without cutting between copies: leave out the cut marks between labels
    #[arg(long)]
    no_cut_marks: bool,
    /// Number of labels to print
    #[arg(long, default_value_t = 1)]
    copies: u32,
    /// Blank tape before and after the content, in mm [default: 2]
    #[arg(long)]
    margin: Option<f32>,
    /// Space between elements, in mm [default: 1.5]
    #[arg(long)]
    gap: Option<f32>,
    /// Minimum label length in mm
    #[arg(long)]
    min_length: Option<f32>,
    /// Extra blank space at the top and bottom edge, in mm
    #[arg(long)]
    padding: Option<f32>,
    /// Frame around the content
    #[arg(long, value_enum)]
    border: Option<BorderArg>,
    /// Rotate the label by 180 degrees on the tape
    #[arg(long)]
    rotate: bool,
    /// With --server: name shown in the service's job list
    #[arg(long, env = "LWPRINT_NAME")]
    name: Option<String>,
    /// With --server: return as soon as the job is queued instead of waiting for it
    #[arg(long)]
    no_wait: bool,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Status if cli.server.is_some() => server_status(&cli).await,
        Cmd::Status => {
            let (_p, addr, st) = open(&cli).await?;
            println!("Printer:  {addr}");
            match LW_C410.tape_by_code(st.tape_code()) {
                Some(t) => println!("Tape:     {} mm ({} pixels)", t.mm, t.pixels),
                None => println!("Tape:     none or unknown (code {:02X})", st.tape_code()),
            }
            println!("State:    {}", printer::describe_problem(&st));
            let raw: Vec<String> = st.fields.iter().map(|(k, v)| format!("{k}:{v:02X}")).collect();
            println!("Raw:      {}", raw.join(" "));
            Ok(())
        }
        Cmd::Print(args) => print(&cli, args).await,
        Cmd::Serve { config } => serve(config).await,
        Cmd::Icons { query, emoji, limit } => {
            let q = query.join(" ");
            if *emoji {
                for h in icons::search_emoji(&q, *limit) {
                    println!("{}  {} ({})", h.emoji, h.name, h.group);
                }
            } else {
                for h in icons::search_icons(&q, *limit) {
                    println!("{}", h.name);
                }
            }
            Ok(())
        }
    }
}

async fn open(cli: &Cli) -> Result<(Printer, Address, lw_core::Status)> {
    let addr: Address = cli
        .device
        .as_deref()
        .ok_or_else(|| anyhow!("no printer given; use --device or set LWPRINT_DEVICE"))?
        .parse()
        .context("invalid Bluetooth address")?;
    // Remember the channel after a successful lookup: fewer connections, fewer chances for
    // the link races described in docs/PROTOCOL.md.
    let cached = if cli.channel.is_none() { read_cached_channel(&addr) } else { None };
    let result = Printer::open(addr, cli.channel.or(cached), cli.verbose).await;
    let (p, channel, st) = match result {
        Ok(v) => v,
        Err(e) => {
            if cached.is_some() {
                forget_cached_channel(&addr);
            }
            return Err(e.context("could not connect to the printer (is it switched on and in range?)"));
        }
    };
    if cli.channel.is_none() && cached != Some(channel) {
        write_cached_channel(&addr, channel);
    }
    if cli.verbose {
        eprintln!("using channel {channel}");
    }
    Ok((p, addr, st))
}

fn channel_cache_file(addr: &Address) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("lwprint").join(format!("{}.channel", addr.to_string().replace(':', ""))))
}

fn read_cached_channel(addr: &Address) -> Option<u8> {
    std::fs::read_to_string(channel_cache_file(addr)?).ok()?.trim().parse().ok()
}

fn write_cached_channel(addr: &Address, channel: u8) {
    if let Some(f) = channel_cache_file(addr) {
        let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(f, channel.to_string());
    }
}

fn forget_cached_channel(addr: &Address) {
    if let Some(f) = channel_cache_file(addr) {
        let _ = std::fs::remove_file(f);
    }
}

async fn print(cli: &Cli, args: &PrintArgs) -> Result<()> {
    let (specs, base_dir) = build_spec(args)?;
    let needs_font =
        specs.iter().flat_map(|s| &s.elements).any(|e| matches!(e.element, Element::Text { .. }));
    let fonts = if needs_font { vec![load_font(args.font.as_deref())?, icons::emoji_font()] } else { vec![] };
    let icon_font = icons::icon_font();
    let notes = std::cell::RefCell::new(Vec::<String>::new());
    let load_image = |p: &str| -> Result<Vec<u8>, String> {
        if let Some(data) = fonts::decode_data_url(p) {
            return data;
        }
        let path = base_dir.join(p);
        std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))
    };
    let requested_tape = match args.tape {
        Some(mm) => Some(LW_C410.tape_by_mm(mm).ok_or_else(|| {
            anyhow!("unsupported tape width {mm} mm (LW-C410: 4, 6, 9, 12, 18)")
        })?),
        None => None,
    };

    let orientation = if args.rotate { Orientation::Rotated } else { Orientation::Normal };
    let cut = match (args.no_cut, args.cut) {
        (true, _) | (_, CutArg::None) => lw_core::CutMode::None,
        (_, CutArg::End) => lw_core::CutMode::End,
        (_, CutArg::Each) => lw_core::CutMode::Each,
    };
    let opts = JobOptions { density: args.density, cut };
    let copies = args.copies.max(1) as usize;

    if let (Some(url), None, None) = (&cli.server, &args.preview, &args.dump) {
        return print_via_server(cli, url, args, specs, &base_dir, cut).await;
    }

    // Render every label for a tape and lay them out as the printer job's pages.
    let build = |tape: lw_core::Tape| -> Result<Vec<printer::Page>> {
        let ctx = RenderContext {
            model: LW_C410,
            tape,
            fonts: &fonts,
            icons: Some(&icon_font),
            load_image: &load_image,
            notes: Some(&notes),
        };
        let labels = specs
            .iter()
            .map(|s| render(s, &ctx).map(|b| (b, copies)))
            .collect::<Result<Vec<_>, _>>()?;
        print_notes(&notes);
        Ok(printer::batch_pages(&labels, cut, !args.no_cut_marks))
    };

    if args.preview.is_some() || args.dump.is_some() {
        let tape = requested_tape.unwrap_or(LW_C410.tape_by_mm(12).unwrap());
        let pages = build(tape)?;
        if let Some(out) = &args.preview {
            // What comes out of the printer: padded to the minimum page length, and all
            // labels on one strip when they are not cut apart.
            let page = &pages[0].bitmap;
            let img = image::GrayImage::from_raw(page.width() as u32, page.height() as u32, page.to_gray())
                .context("could not build preview image")?;
            img.save(out).with_context(|| format!("could not save {}", out.display()))?;
            let more = if pages.len() > 1 { format!("; first of {} pages", pages.len()) } else { String::new() };
            println!("Saved {} ({} mm tape, {:.0} mm of tape{more})", out.display(), tape.mm, length_mm(page.width()));
        }
        if let Some(out) = &args.dump {
            let bitmaps: Vec<_> = pages.into_iter().map(|p| p.bitmap).collect();
            let bytes: Vec<u8> =
                printer::job_chunks(&bitmaps, &opts, orientation).into_iter().flat_map(|c| c.data).collect();
            std::fs::write(out, &bytes).with_context(|| format!("could not save {}", out.display()))?;
            println!("Saved print job ({} bytes, {} mm tape) to {}", bytes.len(), tape.mm, out.display());
        }
        return Ok(());
    }

    let (mut p, _, st) = open(cli).await?;
    let tape = LW_C410
        .tape_by_code(st.tape_code())
        .ok_or_else(|| anyhow!("no supported tape detected (code {:02X})", st.tape_code()))?;
    if let Some(req) = requested_tape {
        if req.mm != tape.mm {
            bail!("--tape {} mm given, but {} mm tape is inserted", req.mm, tape.mm);
        }
    }
    let pages = build(tape)?;
    let bitmaps: Vec<_> = pages.iter().map(|p| p.bitmap.clone()).collect();
    let total = bitmaps.len();
    let settle = std::time::Duration::from_millis(args.settle_ms);
    let out = p
        .print_pages(&bitmaps, &opts, orientation, settle, &|_| true, &mut |i| {
            if total > 1 {
                eprintln!("sent page {} of {total}", i + 1);
            }
        })
        .await?;
    if let Some(warning) = out.warning {
        eprintln!("warning: {warning}");
    }
    let labels = specs.len() * copies;
    let mm: f32 = bitmaps.iter().map(|b| length_mm(b.width())).sum();
    println!(
        "Printed {} on {} mm tape ({:.0} mm of tape)",
        if labels == 1 { "1 label".to_string() } else { format!("{labels} labels") },
        tape.mm,
        mm
    );
    Ok(())
}

fn print_notes(notes: &std::cell::RefCell<Vec<String>>) {
    for n in notes.borrow_mut().drain(..) {
        eprintln!("note: {n}");
    }
}

fn length_mm(px: usize) -> f32 {
    px as f32 * 25.4 / LW_C410.dpi as f32
}

/// Build the label from --spec or from the individual options.
/// Returns the spec and the directory that relative image paths are resolved against.
fn build_spec(args: &PrintArgs) -> Result<(Vec<LabelSpec>, PathBuf)> {
    let (mut specs, base) = if let Some(path) = &args.spec {
        let (json, base) = if path.as_os_str() == "-" {
            (std::io::read_to_string(std::io::stdin())?, PathBuf::from("."))
        } else {
            let json = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let base = path.parent().map(Path::to_path_buf).unwrap_or_else(|| ".".into());
            (json, base)
        };
        // One label, or a list of labels that print together as one job.
        let value: serde_json::Value = serde_json::from_str(&json).context("invalid label JSON")?;
        let specs = if value.is_array() {
            serde_json::from_value::<Vec<LabelSpec>>(value).context("invalid label JSON")?
        } else {
            vec![serde_json::from_value::<LabelSpec>(value).context("invalid label JSON")?]
        };
        if specs.is_empty() {
            bail!("the label list is empty");
        }
        (specs, base)
    } else {
        let mut elements = Vec::new();
        if let Some(img) = &args.image {
            elements.push(Element::Image {
                path: img.to_string_lossy().into_owned(),
                dither: args.dither,
                height_mm: None,
            });
        }
        for name in &args.icon {
            elements.push(Element::Icon { name: name.clone(), size_mm: None });
        }
        if let Some(data) = &args.qr {
            elements.push(Element::Qr { data: data.clone(), size_mm: None, fill: !args.qr_equal });
        }
        if !args.text.is_empty() {
            elements.push(Element::Text {
                text: args.text.join("\n"),
                size_mm: args.text_size,
                align: lw_core::HAlign::Center,
            });
        }
        if let Some(data) = &args.barcode {
            let symbology = match args.barcode_type {
                BarcodeType::Code128 => Symbology::Code128,
                BarcodeType::Ean13 => Symbology::Ean13,
                BarcodeType::Code39 => Symbology::Code39,
            };
            elements.push(Element::Barcode { data: data.clone(), symbology, module_px: 2, height_mm: None });
        }
        if elements.is_empty() {
            bail!("nothing to print; use --text, --qr, --barcode, --image or --spec");
        }
        (vec![LabelSpec::new(elements)], PathBuf::from("."))
    };
    for spec in &mut specs {
        if let Some(m) = args.margin {
            spec.margin_mm = m;
        }
        if let Some(g) = args.gap {
            spec.gap_mm = g;
        }
        if args.min_length.is_some() {
            spec.min_length_mm = args.min_length;
        }
        if let Some(p) = args.padding {
            spec.padding_mm = p;
        }
        if let Some(b) = args.border {
            spec.border = match b {
                BorderArg::None => lw_core::Border::None,
                BorderArg::Line => lw_core::Border::Line,
                BorderArg::Double => lw_core::Border::Double,
                BorderArg::Rounded => lw_core::Border::Rounded,
                BorderArg::Dashed => lw_core::Border::Dashed,
            };
        }
    }
    Ok((specs, base))
}

async fn serve(config_path: &Path) -> Result<()> {
    let config = config::Config::load(config_path)?;
    let fonts = match load_font(config.printer.font.as_deref()) {
        Ok(f) => vec![f, icons::emoji_font()],
        Err(e) => {
            service::log(format!("warning: {e:#}; text labels will fail"));
            vec![]
        }
    };
    service::log(format!("lwprint {} starting, printer {}", env!("CARGO_PKG_VERSION"), config.printer.address));
    let mqtt_cfg = config.mqtt.clone();
    let svc = service::Service::start(config, fonts, icons::icon_font())?;
    if let Some(m) = mqtt_cfg {
        tokio::spawn(mqtt::run(svc.clone(), m));
    }
    tokio::select! {
        r = http::serve(svc) => r,
        _ = tokio::signal::ctrl_c() => {
            service::log("stopping");
            Ok(())
        }
    }
}

fn client(cli: &Cli, url: &str) -> Result<client::Client> {
    client::Client::new(url, cli.token.clone())
}

async fn server_status(cli: &Cli) -> Result<()> {
    let url = cli.server.as_deref().unwrap_or_default();
    let (code, st) = client(cli, url)?.request("GET", "/api/status", None).await?;
    if code != 200 {
        return Err(client::error_of(code, &st));
    }
    let online = st["printer_online"].as_bool().unwrap_or(false);
    println!("Service:  {url}");
    println!("Printer:  {}", if online { "reachable" } else { "switched off or out of range" });
    if let Some(mm) = st["tape_mm"].as_u64() {
        println!("Tape:     {mm} mm");
    }
    println!("State:    {}", st["state"].as_str().unwrap_or("unknown"));
    if let Some(p) = st["problem"].as_str() {
        println!("Problem:  {p}");
    }
    println!("Waiting:  {}", st["queue"].as_u64().unwrap_or(0));
    Ok(())
}

/// Replace image file paths with data: URLs, since the service never reads files.
fn embed_images(spec: &mut LabelSpec, base_dir: &Path) -> Result<()> {
    use base64::Engine;
    for item in &mut spec.elements {
        if let Element::Image { path, .. } = &mut item.element {
            if path.starts_with("data:") {
                continue;
            }
            let file = base_dir.join(&*path);
            let bytes = std::fs::read(&file).with_context(|| format!("could not read {}", file.display()))?;
            let mime = match file.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
                Some("png") => "image/png",
                Some("jpg") | Some("jpeg") => "image/jpeg",
                _ => "application/octet-stream",
            };
            *path = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes));
        }
    }
    Ok(())
}

async fn print_via_server(
    cli: &Cli,
    url: &str,
    args: &PrintArgs,
    specs: Vec<LabelSpec>,
    base_dir: &Path,
    cut: lw_core::CutMode,
) -> Result<()> {
    let user = std::env::var("USER").unwrap_or_else(|_| "cli".into());
    let mut reqs = Vec::new();
    for mut spec in specs {
        embed_images(&mut spec, base_dir)?;
        let mut req = serde_json::to_value(&spec)?;
        req["copies"] = args.copies.max(1).into();
        req["density"] = args.density.into();
        req["cut"] = serde_json::to_value(cut)?;
        req["rotate"] = args.rotate.into();
        req["cut_marks"] = (!args.no_cut_marks).into();
        if let Some(mm) = args.tape {
            req["tape_mm"] = mm.into();
        }
        req["requested_by"] = args.name.clone().unwrap_or_else(|| user.clone()).into();
        reqs.push(req);
    }
    let body = if reqs.len() == 1 { reqs.remove(0) } else { serde_json::Value::Array(reqs) };

    let c = client(cli, url)?;
    let (code, reply) = c.request("POST", "/api/print", Some(&body)).await?;
    if code != 202 {
        return Err(client::error_of(code, &reply));
    }
    let jobs: Vec<serde_json::Value> = match reply {
        serde_json::Value::Array(a) => a,
        one => vec![one],
    };
    let ids: Vec<u64> = jobs.iter().filter_map(|j| j["id"].as_u64()).collect();
    if ids.is_empty() {
        bail!("the service returned no job id");
    }
    let list: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    println!("Queued as job{} {} on {url}", if ids.len() > 1 { "s" } else { "" }, list.join(", "));
    if args.no_wait {
        return Ok(());
    }

    // Follow the jobs until all have finished; Ctrl+C asks the service to stop them.
    let mut last: std::collections::HashMap<u64, String> = Default::default();
    let mut open: Vec<u64> = ids.clone();
    let mut failures = Vec::new();
    let mut stop_requested = false;
    while !open.is_empty() {
        for id in open.clone() {
            let (code, job) = c.request("GET", &format!("/api/jobs/{id}"), None).await?;
            if code != 200 {
                return Err(client::error_of(code, &job));
            }
            let state = job["state"].as_str().unwrap_or("unknown").to_string();
            let message = job["message"].as_str().unwrap_or("").to_string();
            let line = if message.is_empty() { state.clone() } else { format!("{state}: {message}") };
            let prefix = if ids.len() > 1 { format!("job {id}: ") } else { String::new() };
            if last.get(&id) != Some(&line) {
                match state.as_str() {
                    "done" => println!("{prefix}printed: {message}"),
                    "failed" | "cancelled" => {
                        eprintln!("{prefix}{line}");
                        failures.push(format!("job {id} {state}: {message}"));
                    }
                    _ => eprintln!("{prefix}{line}"),
                }
                last.insert(id, line);
            }
            if matches!(state.as_str(), "done" | "failed" | "cancelled") {
                open.retain(|x| *x != id);
            }
        }
        if open.is_empty() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(800)) => {}
            _ = tokio::signal::ctrl_c(), if !stop_requested => {
                stop_requested = true;
                eprintln!("stopping...");
                for id in &open {
                    let _ = c.request("POST", &format!("/api/jobs/{id}/cancel"), None).await;
                }
            }
        }
    }
    match failures.len() {
        0 => Ok(()),
        _ => bail!("{}", failures.join("; ")),
    }
}
