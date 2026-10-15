//! Core of `lwprint`: label rendering and the Epson LabelWorks printer protocol.
//! Contains no Bluetooth or other I/O code, so it also builds for WebAssembly.

pub mod bitmap;
pub mod model;
pub mod protocol;
pub mod render;

pub use bitmap::Bitmap;
pub use model::{Model, Tape, LW_C410};
pub use protocol::{CutMode, JobOptions, Orientation, Status, StatusReader};
pub use render::{
    render, Border, Element, HAlign, IconFont, Item, LabelSpec, RenderContext, RenderError, Symbology, VAlign,
};
