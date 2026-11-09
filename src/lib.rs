//! Batch photo importer: walks a source folder, reads the shooting date from each
//! image's metadata and files the image under `<destination>/<year>/<year-month-day>/`.

pub mod app;
pub mod importer;
pub mod metadata;
pub mod settings;
pub mod ui_gpui;
