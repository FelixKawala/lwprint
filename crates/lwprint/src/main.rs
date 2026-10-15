//! `lwprint`: print labels on an Epson LabelWorks LW-C410 over Bluetooth.

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
        Cmd::Status => {
            let (mut p, addr, ch) = open(&cli).await?;
            let st = p.status().await?;
            println!("Printer:  {addr} (channel {ch})");
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

async fn open(cli: &Cli) -> Result<(Printer, Address, u8)> {
    let addr: Address = cli
        .device
        .as_deref()
        .ok_or_else(|| anyhow!("no printer given; use --device or set LWPRINT_DEVICE"))?
        .parse()
        .context("invalid Bluetooth address")?;
    let channel = match cli.channel {
        Some(c) => c,
        None => sdp::find_serial_channel(addr)
            .await
            .context("could not look up the printer's serial port channel (try --channel 6)")?,
    };
    Ok((Printer::connect(addr, channel, cli.verbose).await?, addr, channel))
}

async fn print(cli: &Cli, args: &PrintArgs) -> Result<()> {
    let (spec, base_dir) = build_spec(args)?;
    let needs_font = spec.elements.iter().any(|e| matches!(e.element, Element::Text { .. }));
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

    if args.preview.is_some() || args.dump.is_some() {
        let tape = requested_tape.unwrap_or(LW_C410.tape_by_mm(12).unwrap());
        let ctx = RenderContext {
            model: LW_C410,
            tape,
            fonts: &fonts,
            icons: Some(&icon_font),
            load_image: &load_image,
            notes: Some(&notes),
        };
        let label = render(&spec, &ctx)?;
        print_notes(&notes);
        if let Some(out) = &args.preview {
            let img = image::GrayImage::from_raw(label.width() as u32, label.height() as u32, label.to_gray())
                .context("could not build preview image")?;
            img.save(out).with_context(|| format!("could not save {}", out.display()))?;
            println!(
                "Saved {} ({} mm tape, label {:.0} mm long)",
                out.display(),
                tape.mm,
                length_mm(label.width())
            );
        }
        if let Some(out) = &args.dump {
            let pages = vec![label.clone(); args.copies.max(1) as usize];
            let bytes: Vec<u8> = printer::job_chunks(&pages, &opts, orientation)
                .into_iter()
                .flat_map(|c| c.data)
                .collect();
            std::fs::write(out, &bytes).with_context(|| format!("could not save {}", out.display()))?;
            println!("Saved print job ({} bytes, {} mm tape) to {}", bytes.len(), tape.mm, out.display());
        }
        return Ok(());
    }

    let (mut p, _, _) = open(cli).await?;
    let st = p.status().await?;
    let tape = LW_C410
        .tape_by_code(st.tape_code())
        .ok_or_else(|| anyhow!("no supported tape detected (code {:02X})", st.tape_code()))?;
    if let Some(req) = requested_tape {
        if req.mm != tape.mm {
            bail!("--tape {} mm given, but {} mm tape is inserted", req.mm, tape.mm);
        }
    }
    let ctx = RenderContext {
            model: LW_C410,
            tape,
            fonts: &fonts,
            icons: Some(&icon_font),
            load_image: &load_image,
            notes: Some(&notes),
        };
    let label = render(&spec, &ctx)?;
    let settle = std::time::Duration::from_millis(args.settle_ms);
    let copies = args.copies.max(1) as usize;
    let pages = vec![label.clone(); copies];
    let out = p
        .print_pages(&pages, &opts, orientation, settle, &|| false, &mut |n| {
            if copies > 1 {
                eprintln!("sent label {n} of {copies}");
            }
        })
        .await?;
    if let Some(warning) = out.warning {
        eprintln!("warning: {warning}");
    }
    println!(
        "Printed {} ({} mm tape, {:.0} mm long each)",
        if copies == 1 { "1 label".to_string() } else { format!("{copies} labels") },
        tape.mm,
        length_mm(label.width())
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
fn build_spec(args: &PrintArgs) -> Result<(LabelSpec, PathBuf)> {
    let (mut spec, base) = if let Some(path) = &args.spec {
        let (json, base) = if path.as_os_str() == "-" {
            (std::io::read_to_string(std::io::stdin())?, PathBuf::from("."))
        } else {
            let json = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let base = path.parent().map(Path::to_path_buf).unwrap_or_else(|| ".".into());
            (json, base)
        };
        (serde_json::from_str::<LabelSpec>(&json).context("invalid label JSON")?, base)
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
        (LabelSpec::new(elements), PathBuf::from("."))
    };
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
    Ok((spec, base))
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
