//! Drawing the window: a [state::View] on the screen, and the presses that call the library back.
//!
//! Nothing here works out what a state means - [state] does that, and its tests are the ones the
//! brief asks for. This file lays those words out, and every press hands the work to a [task]
//! worker so the UI thread never waits on a download, a hash or a card probe ([facts]).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Button, CentralPanel, Color32, Context, FontData, FontDefinitions, FontFamily, FontId, Frame, Label, Layout, Margin, Rangef, Response, RichText,
    ScrollArea, Sense, Stroke, StrokeKind, TextStyle, Ui, Vec2, ViewportBuilder, ViewportCommand, Visuals,
};

use crate::error::{Fail, Result};
use crate::server::{Server, StopHandle};
use crate::window::facts::{Busy, Source};
use crate::window::format;
use crate::window::state;
use crate::window::task;

/// The window's ground: warm paper rather than white, so the ink reads as print.
const GROUND: Color32 = Color32::from_rgb(0xF4, 0xF2, 0xED);
/// Everything the window says.
const INK: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x18);
/// The second voice: meta lines, explanations, quiet buttons.
const GREY: Color32 = Color32::from_rgb(0x6B, 0x6A, 0x65);
/// Hairlines, and the unfilled part of a progress bar.
const RULE: Color32 = Color32::from_rgb(0xD9, 0xD5, 0xCC);
/// The one loud colour: the accent bar, the needs-you dot, a link.
const ACCENT: Color32 = Color32::from_rgb(0xB4, 0x40, 0x0C);
/// The primary button's face.
const WHITE: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);

/// The status sentence, in the brief's 28 px.
const STATUS: f32 = 28.0;
/// Everything the window says at reading size.
const BODY: f32 = 15.0;
/// The 12 px voice: meta, explanations, Details.
const META: f32 = 12.0;

/// How wide the column of words is, whatever the window is.
const COLUMN: f32 = 560.0;
/// The margin either side of it.
const SIDE: i8 = 40;
/// The margin above and below.
const TOP: i8 = 48;
/// The height of the one filled button on the screen.
const PRIMARY: f32 = 48.0;
/// The height of an outlined button.
const SECONDARY: f32 = 40.0;
/// The height of a word with no frame around it.
const TERTIARY: f32 = 24.0;
/// How round a button is.
const RADIUS: f32 = 4.0;
/// The focus ring the brief asks for.
const RING: f32 = 2.0;
/// The accent bar down the left of a note or a problem.
const BAR: f32 = 2.0;
/// The gap between that bar and the words beside it.
const PAD: f32 = 14.0;
/// The gap between a row's words and its state on the right.
const GAP: f32 = 20.0;
/// How wide the right-hand column of a row is.
const STATE: f32 = 190.0;
/// The brief's rule 3: never repaint faster than this.
const REPAINT: Duration = Duration::from_millis(100);
/// The bold voice. Windows draws it from `seguisb.ttf`; without that file it falls back to the
/// regular face, which is why the family list below always ends with `segoeui`.
const SEMIBOLD: &str = "Segoe UI Semibold";
/// The Details lines that are paths, addresses and raw errors: monospace, per the brief.
const MONOSPACE: [&str; 4] = ["Models folder", "Address", "Connection file", "What happened"];
/// The id [facts] reports CUDA progress under, which is not a catalog model.
const CUDA_ID: &str = "cuda";

/// The window: the last [state::View] it drew, and the small amount of screen state the brief's
/// interaction needs (which note is open, which box is ticked, whether Details is showing).
pub struct Window {
    source: Arc<Source>,
    /// The server's own stop flag, which is how closing the window stops CAIM (brief rule 4).
    stop: Arc<AtomicBool>,
    facts: state::Facts,
    view: state::View,
    /// The row whose licence note is open under it.
    open_note: Option<String>,
    /// That note's box, which is what enables its Download button.
    note_ticked: bool,
    /// Section 4.4 row 3 has its own box, because its button stays disabled until it is ticked.
    action_ticked: bool,
    /// The offer's terms, and their box.
    cuda_open: bool,
    cuda_ticked: bool,
    /// Whether "Read NVIDIA's terms" has been pressed.
    cuda_terms: bool,
    details: bool,
    /// The close question is showing, or the window has been asked to go.
    closing: bool,
    running: Option<task::Running>,
    /// A sentence the window has to say for itself: a press that could not start, or a folder that
    /// would not open. Plain words only (brief rule 6).
    message: Option<String>,
}

impl Window {
    /// The window for one running manager.
    fn new(source: Arc<Source>, stop: Arc<AtomicBool>) -> Window {
        let facts = source.gather();
        let view = state::view(&facts);
        Window {
            source,
            stop,
            facts,
            view,
            open_note: None,
            note_ticked: false,
            action_ticked: false,
            cuda_open: false,
            cuda_ticked: false,
            cuda_terms: false,
            details: false,
            closing: false,
            running: None,
            message: None,
        }
    }
}

/// Open the window and stay inside it until it closes or the server is asked to stop.
///
/// This is what `caim open` does. The server is already listening by the time this is called, and
/// the window is the only way back out. A machine with no models folder, a catalog that will not
/// parse, no readable system font or no usable graphics device gets a sentence instead, and the
/// caller keeps serving in the terminal.
pub fn open(server: &mut Server, stop: &StopHandle) -> Result<()> {
    let source = Arc::new(Source::new(server.runner(), server.port(), server.endpoint_path().map(Path::to_path_buf), server.routing())?);
    let fonts = system_fonts()?;
    let stop_flag = stop.stop_signal();
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default().with_title(format::WINDOW_TITLE).with_inner_size([640.0, 800.0]).with_min_inner_size([480.0, 560.0]),
        ..Default::default()
    };
    let ran = eframe::run_native(
        format::WINDOW_TITLE,
        options,
        Box::new(move |context| {
            context.egui_ctx.set_fonts(fonts);
            apply_style(&context.egui_ctx);
            Ok(Box::new(Window::new(source, stop_flag)))
        }),
    );
    ran.map_err(|problem| Fail::internal(format!("The Craft AI Manager window could not open on this computer ({problem}).")))
}

/// The fonts the window draws with: the ones Windows itself uses, read from the system folder.
///
/// Nothing is bundled - the brief leaves that to Erik - so a machine whose own font files cannot be
/// read gets a sentence, and the caller serves in the terminal instead.
fn system_fonts() -> Result<FontDefinitions> {
    let folder = fonts_folder();
    let Some(regular) = font_file(&folder, "segoeui.ttf") else {
        return Err(Fail::internal("This computer's own fonts could not be read, so the window cannot open."));
    };
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("segoeui".to_string(), Arc::new(FontData::from_owned(regular)));
    let mut proportional = Vec::new();
    if let Some(semibold) = font_file(&folder, "seguisb.ttf") {
        fonts.font_data.insert(SEMIBOLD.to_string(), Arc::new(FontData::from_owned(semibold)));
        proportional.push(SEMIBOLD.to_string());
    }
    proportional.push("segoeui".to_string());
    fonts.families.insert(FontFamily::Proportional, proportional.clone());
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), proportional);
    let mut monospace = Vec::new();
    if let Some(consolas) = font_file(&folder, "consola.ttf") {
        fonts.font_data.insert("consolas".to_string(), Arc::new(FontData::from_owned(consolas)));
        monospace.push("consolas".to_string());
    }
    monospace.push("segoeui".to_string());
    fonts.families.insert(FontFamily::Monospace, monospace);
    Ok(fonts)
}

/// The folder this machine keeps its fonts in.
fn fonts_folder() -> PathBuf {
    fonts_folder_from(std::env::var_os("WINDIR"))
}

/// `WINDIR` names the Windows folder, not the Fonts folder inside it, so the fonts are one level
/// below whatever it points at (MG-13 review round 5). A machine with no `WINDIR` at all gets the
/// folder every Windows install puts its fonts in.
fn fonts_folder_from(windir: Option<std::ffi::OsString>) -> PathBuf {
    windir.map_or_else(|| PathBuf::from(r"C:\Windows\Fonts"), |folder| PathBuf::from(folder).join("Fonts"))
}

/// One font file from the system folder, if this machine has it and it can be read.
fn font_file(folder: &Path, name: &str) -> Option<Vec<u8>> {
    std::fs::read(folder.join(name)).ok()
}

/// The brief's look: light theme, warm paper, ink text, and its three text sizes.
fn apply_style(context: &Context) {
    context.set_theme(egui::ThemePreference::Light);
    let mut visuals = Visuals::light();
    visuals.panel_fill = GROUND;
    visuals.window_fill = GROUND;
    visuals.extreme_bg_color = WHITE;
    visuals.faint_bg_color = RULE;
    visuals.override_text_color = Some(INK);
    visuals.hyperlink_color = ACCENT;
    visuals.selection.bg_fill = RULE;
    visuals.selection.stroke = Stroke::new(1.0, INK);
    for widget in [&mut visuals.widgets.noninteractive, &mut visuals.widgets.inactive, &mut visuals.widgets.hovered, &mut visuals.widgets.active] {
        widget.fg_stroke = Stroke::new(1.0, INK);
        widget.bg_stroke = Stroke::new(1.0, RULE);
        widget.weak_bg_fill = GROUND;
    }
    context.set_visuals_of(egui::Theme::Light, visuals);
    context.style_mut_of(egui::Theme::Light, |style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(META, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(BODY, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(BODY, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(META, FontFamily::Monospace)),
            (TextStyle::Heading, FontId::new(STATUS, FontFamily::Name(SEMIBOLD.into()))),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(0.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 11.0);
    });
}
impl eframe::App for Window {
    /// Every repaint starts here, on the UI thread: ask for the card if it is due, read the state
    /// model, let go of a worker that has finished, and settle what closing means.
    fn logic(&mut self, context: &Context, _frame: &mut eframe::Frame) {
        self.source.probe_card();
        self.facts = self.source.gather();
        self.view = state::view(&self.facts);
        if self.running.as_ref().is_some_and(|running| running.is_finished())
            && let Some(mut running) = self.running.take()
        {
            running.join();
        }
        let asked = context.input(|input| input.viewport().close_requested());
        if asked && !self.closing && self.facts.running.is_some() {
            // Brief rule 4: closing while PhotoCraft has a job in flight asks once. The part files
            // of a download this window started are kept either way (brief rule 3).
            self.closing = true;
            context.send_viewport_cmd(ViewportCommand::CancelClose);
        }
        if self.stop.load(Ordering::Relaxed) && !asked {
            context.send_viewport_cmd(ViewportCommand::Close);
        }
        context.request_repaint_after(REPAINT);
    }

    fn clear_color(&self, _visuals: &Visuals) -> [f32; 4] {
        GROUND.to_normalized_gamma_f32()
    }

    /// Wait for a worker on the way out, so a download is not cut between one write and the next.
    fn on_exit(&mut self) {
        if let Some(running) = self.running.as_mut() {
            running.cancel();
            running.join();
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        CentralPanel::default().frame(Frame::NONE.fill(GROUND).inner_margin(Margin::symmetric(SIDE, TOP))).show(ui, |ui| {
            let width = ui.available_width().min(COLUMN);
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Center), |ui| {
                    self.column(ui);
                });
            });
        });
    }
}

impl Window {
    /// The column, top to bottom, in the brief's order.
    fn column(&mut self, ui: &mut Ui) {
        self.status(ui);
        if let Some(message) = self.message.clone() {
            ui.add_space(12.0);
            ui.add(Label::new(RichText::new(message).size(META).color(INK)));
        }
        if self.closing {
            ui.add_space(24.0);
            self.close_question(ui);
        }
        if let Some(action) = self.view.action.clone() {
            ui.add_space(24.0);
            self.action(ui, &action);
        }
        ui.add_space(28.0);
        self.rows(ui);
        if let Some(offer) = self.view.offer.clone() {
            ui.add_space(28.0);
            self.offer(ui, &offer);
        }
        if self.details {
            ui.add_space(28.0);
            self.details(ui);
        }
        ui.add_space(28.0);
        self.footer(ui);
    }

    /// The status block: the bold sentence, the grey one under it, and the 12 px line.
    fn status(&mut self, ui: &mut Ui) {
        let width = ui.available_width();
        let mut job = LayoutJob::simple(self.view.bold.clone(), FontId::new(STATUS, FontFamily::Name(SEMIBOLD.into())), INK, width);
        job.append(&format!("\n{}", self.view.grey), 0.0, TextFormat::simple(FontId::proportional(BODY), GREY));
        ui.add(Label::new(job));
        ui.add_space(6.0);
        ui.add(Label::new(RichText::new(self.view.meta.clone()).size(META).color(GREY)));
    }

    /// Section 4, rule 4: the one question asked when a job is running.
    fn close_question(&mut self, ui: &mut Ui) {
        self.accent(ui, |window, ui| {
            ui.add(Label::new(RichText::new(format::CLOSE_QUESTION).size(BODY).color(INK)));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if window.primary(ui, format::CLOSE_WORDS, true).clicked() {
                    window.stop.store(true, Ordering::Relaxed);
                }
                ui.add_space(10.0);
                if window.tertiary(ui, format::KEEP_OPEN_WORDS).clicked() {
                    window.closing = false;
                }
            });
        });
    }

    /// A block with the brief's 2 px accent bar down its left side: a note, NVIDIA's terms, or the
    /// one problem the window is showing.
    fn accent(&mut self, ui: &mut Ui, content: impl FnOnce(&mut Self, &mut Ui)) {
        let width = ui.available_width();
        let response = ui
            .allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::left_to_right(Align::Min), |ui| {
                ui.add_space(BAR + PAD);
                ui.allocate_ui_with_layout(Vec2::new(width - BAR - PAD, 0.0), Layout::top_down(Align::Min).with_cross_justify(true), |ui| {
                    content(self, ui);
                })
                .response
            })
            .response;
        let rect = response.rect;
        ui.painter().vline(rect.min.x + BAR / 2.0, Rangef::new(rect.min.y, rect.max.y), Stroke::new(BAR, ACCENT));
    }
}
impl Window {
    /// Every catalog entry is always listed (brief section 3), so the rows are the whole catalog.
    fn rows(&mut self, ui: &mut Ui) {
        let rows = self.view.rows.clone();
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                ui.add_space(16.0);
                self.hairline(ui);
                ui.add_space(16.0);
            }
            self.row(ui, row);
            if self.open_note.as_deref() == Some(row.id.as_str()) {
                ui.add_space(14.0);
                self.note_block(ui, &row.id);
            }
        }
    }

    /// The 1 px rule between two feature rows.
    fn hairline(&mut self, ui: &mut Ui) {
        let width = ui.available_width();
        let response = ui.allocate_response(Vec2::new(width, 1.0), Sense::hover());
        ui.painter().hline(response.rect.x_range(), response.rect.center().y, Stroke::new(1.0, RULE));
    }

    /// One feature row: title, purpose and meta on the left, its state on the right.
    fn row(&mut self, ui: &mut Ui, row: &state::Row) {
        let width = ui.available_width();
        ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::left_to_right(Align::Min), |ui| {
            ui.allocate_ui_with_layout(Vec2::new(width - STATE - GAP, 0.0), Layout::top_down(Align::Min).with_cross_justify(true), |ui| {
                ui.add(Label::new(RichText::new(row.title.clone()).size(BODY).strong().color(INK)));
                ui.add_space(2.0);
                ui.add(Label::new(RichText::new(row.purpose.clone()).size(BODY).color(INK)));
                ui.add_space(4.0);
                ui.add(Label::new(RichText::new(row.meta.clone()).size(META).color(GREY)));
            });
            ui.add_space(GAP);
            ui.allocate_ui_with_layout(Vec2::new(STATE, 0.0), Layout::top_down(Align::Max).with_cross_justify(true), |ui| {
                self.row_state(ui, row);
            });
        });
    }

    /// The right-hand side of a row, which is the only part of it that does anything.
    fn row_state(&mut self, ui: &mut Ui, row: &state::Row) {
        match &row.state {
            state::RowState::Add { .. } => {
                if self.secondary(ui, &row.state.words()).clicked() {
                    self.add_pressed(row);
                }
            }
            state::RowState::Downloading { percent } => self.downloading(ui, &row.id, *percent, format::MODEL_DOWNLOADING_SUFFIX, STATE),
            other => {
                let words = other.words();
                if other.needs_you() {
                    // The brief's accent dot, in front of the state's own words.
                    let mut job = LayoutJob::simple("\u{25cf} ".to_string(), FontId::proportional(BODY), ACCENT, STATE);
                    job.append(words.trim_start_matches("\u{25cf} "), 0.0, TextFormat::simple(FontId::proportional(BODY), INK));
                    ui.add(Label::new(job));
                } else {
                    ui.add(Label::new(RichText::new(words).size(BODY).color(INK)));
                }
            }
        }
    }

    /// `Add` on a row: the note first when the catalog has one, the download straight away when not.
    fn add_pressed(&mut self, row: &state::Row) {
        let has_note = self.facts.models.iter().any(|model| model.id == row.id && model.note.is_some());
        if has_note {
            self.open_note = Some(row.id.clone());
            self.note_ticked = false;
        } else {
            self.press(task::Task::Install { id: row.id.clone(), accept_note: false });
        }
    }

    /// The licence note, inline under its row (brief 4.1): the catalog's words verbatim, the box,
    /// and the Download button that stays grey until the box is ticked.
    fn note_block(&mut self, ui: &mut Ui, id: &str) {
        let model = self.facts.models.iter().find(|model| model.id == id);
        let note = model.and_then(|model| model.note.clone());
        let bytes = model.map_or(0, |model| model.size_bytes);
        let id = id.to_string();
        self.accent(ui, move |window, ui| {
            ui.add(Label::new(RichText::new(format::NOTE_HEADING).size(BODY).strong().color(INK)));
            ui.add_space(6.0);
            if let Some(note) = note {
                ui.add(Label::new(RichText::new(note).size(BODY).color(INK)));
            }
            ui.add_space(10.0);
            let mut ticked = window.note_ticked;
            ui.checkbox(&mut ticked, format::ACKNOWLEDGE_WORDS);
            window.note_ticked = ticked;
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if window.primary(ui, &format::download_button(bytes), ticked).clicked() {
                    window.download_pressed(&id);
                }
                ui.add_space(10.0);
                if window.tertiary(ui, format::NOT_NOW).clicked() {
                    window.open_note = None;
                    window.note_ticked = false;
                }
            });
        });
    }

    /// The note's Download. The note and its box stay exactly as the person left them unless a
    /// worker actually started, so a refused press cannot take back the tick they just made
    /// (MG-13 review round 5).
    fn download_pressed(&mut self, id: &str) {
        if self.press(task::Task::Install { id: id.to_string(), accept_note: true }) {
            self.open_note = None;
            self.note_ticked = false;
        }
    }

    /// A download in flight: the percentage, the bar, the brief's sentence, and Cancel.
    fn downloading(&mut self, ui: &mut Ui, id: &str, percent: u64, suffix: &str, width: f32) {
        ui.add(Label::new(RichText::new(format!("{percent}%")).size(BODY).color(INK)));
        ui.add_space(6.0);
        self.progress(ui, width, percent as f32 / 100.0);
        if let Some((done, total)) = progress_bytes(&self.source, id) {
            ui.add_space(6.0);
            ui.add(Label::new(RichText::new(format::downloading_line(done, total, suffix)).size(META).color(GREY)));
        }
        ui.add_space(6.0);
        if self.tertiary(ui, format::CANCEL).clicked()
            && let Some(running) = self.running.as_ref()
        {
            running.cancel();
        }
    }

    /// The bar: 2 px of ink over a 1 px rule.
    fn progress(&mut self, ui: &mut Ui, width: f32, fraction: f32) {
        let response = ui.allocate_response(Vec2::new(width, 2.0), Sense::hover());
        let y = response.rect.center().y;
        let left = response.rect.min.x;
        ui.painter().hline(Rangef::new(left, left + width), y, Stroke::new(1.0, RULE));
        let right = left + width * fraction.clamp(0.0, 1.0);
        ui.painter().hline(Rangef::new(left, right), y, Stroke::new(2.0, INK));
    }
}

/// The bytes behind a percentage, so the sentence under the bar can name them.
fn progress_bytes(source: &Source, id: &str) -> Option<(u64, u64)> {
    match source.busy() {
        Some(Busy::Model { id: busy, done, total }) if busy == id => Some((done, total)),
        Some(Busy::Cuda { done, total }) if id == CUDA_ID => Some((done, total)),
        _ => None,
    }
}
impl Window {
    /// The one problem the window is showing (brief 4.4): its note, the box that unlocks its button
    /// when a note has to be read again, the one button, and the 12 px sentence under it.
    fn action(&mut self, ui: &mut Ui, action: &state::Action) {
        let id = action.model.clone();
        let heading = action.heading.clone();
        let note = action.note.clone();
        let button = action.button.clone();
        let explanation = action.explanation.clone();
        let needs_tick = !action.button_enabled;
        self.accent(ui, move |window, ui| {
            if let Some(heading) = heading {
                ui.add(Label::new(RichText::new(heading).size(BODY).strong().color(INK)));
                ui.add_space(6.0);
            }
            if let Some(note) = note {
                ui.add(Label::new(RichText::new(note).size(BODY).color(INK)));
                ui.add_space(10.0);
            }
            if needs_tick {
                let mut ticked = window.action_ticked;
                ui.checkbox(&mut ticked, format::ACKNOWLEDGE_WORDS);
                window.action_ticked = ticked;
                ui.add_space(12.0);
            }
            let enabled = !needs_tick || window.action_ticked;
            if window.primary(ui, &button, enabled).clicked() {
                window.action_pressed(id.as_deref());
            }
            ui.add_space(8.0);
            ui.add(Label::new(RichText::new(explanation).size(META).color(GREY)));
        });
    }

    /// The button on the needs-you block. Which task it starts follows from the problem the state
    /// model picked, so the window does not have to remember it.
    fn action_pressed(&mut self, id: Option<&str>) {
        let Some(id) = id else {
            return;
        };
        let Some(problem) = self.facts.models.iter().find(|model| model.id == id) else {
            return;
        };
        let task = if problem.damaged {
            task::Task::Repair { id: id.to_string() }
        } else if problem.out_of_space || problem.paused.is_some() {
            task::Task::Resume { id: id.to_string() }
        } else {
            task::Task::Acknowledge { id: id.to_string() }
        };
        if self.press(task) {
            self.action_ticked = false;
        }
    }

    /// The graphics-card offer (brief 4.3), which only the state model knows to show.
    fn offer(&mut self, ui: &mut Ui, offer: &state::Offer) {
        ui.add(Label::new(RichText::new(offer.title.clone()).size(BODY).strong().color(INK)));
        ui.add_space(4.0);
        ui.add(Label::new(RichText::new(offer.text.clone()).size(BODY).color(INK)));
        ui.add_space(4.0);
        ui.add(Label::new(RichText::new(offer.meta.clone()).size(META).color(GREY)));
        ui.add_space(12.0);
        let width = ui.available_width();
        match self.source.busy() {
            Some(Busy::Cuda { done, total }) => {
                let percent = format::percent(done, total);
                self.downloading(ui, CUDA_ID, percent, format::CUDA_DOWNLOADING_SUFFIX, width);
            }
            _ => {
                if self.cuda_open {
                    self.cuda_terms(ui, offer.bytes);
                } else if self.secondary(ui, &offer.button).clicked() {
                    self.cuda_open = true;
                }
            }
        }
    }

    /// NVIDIA's terms, inline under the offer: the sentence, the two notes behind the link, the box,
    /// and the Download button. Both notes are the catalog's own words, verbatim.
    fn cuda_terms(&mut self, ui: &mut Ui, bytes: u64) {
        self.accent(ui, move |window, ui| {
            ui.add(Label::new(RichText::new(format::CUDA_TERMS).size(BODY).color(INK)));
            ui.add_space(8.0);
            if window.tertiary(ui, format::CUDA_LINK_WORDS).clicked() {
                window.cuda_terms = !window.cuda_terms;
            }
            if window.cuda_terms {
                for (name, note) in window.source.cuda_notes() {
                    ui.add_space(6.0);
                    ui.add(Label::new(RichText::new(name).size(META).strong().color(INK)));
                    ui.add_space(2.0);
                    ui.add(Label::new(RichText::new(note).size(META).color(GREY)));
                }
            }
            ui.add_space(10.0);
            let mut ticked = window.cuda_ticked;
            ui.checkbox(&mut ticked, format::CUDA_ACCEPT_WORDS);
            window.cuda_ticked = ticked;
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if window.primary(ui, &format::download_button(bytes), ticked).clicked() {
                    window.cuda_download_pressed();
                }
                ui.add_space(10.0);
                if window.tertiary(ui, format::NOT_NOW).clicked() {
                    window.cuda_open = false;
                    window.cuda_ticked = false;
                }
            });
        });
    }

    /// The offer's Download, under the same rule as the licence note: the terms and their box stay
    /// open and ticked unless a worker actually started.
    fn cuda_download_pressed(&mut self) {
        if self.press(task::Task::Cuda { accept_note: true }) {
            self.cuda_open = false;
            self.cuda_ticked = false;
        }
    }

    /// The footer: the brief's sentence, with the Details toggle beside it.
    fn footer(&mut self, ui: &mut Ui) {
        let words = format::footer_without_details(&self.view.footer).to_string();
        ui.horizontal(|ui| {
            ui.add(Label::new(RichText::new(words).size(META).color(GREY)));
            ui.add_space(10.0);
            let toggle = if self.details { format::HIDE_DETAILS_WORDS } else { format::DETAILS_WORDS };
            if self.tertiary(ui, toggle).clicked() {
                self.details = !self.details;
            }
        });
    }

    /// The Details list (brief 4.5), with the buttons that belong beside each line.
    fn details(&mut self, ui: &mut Ui) {
        let installed: Vec<String> = self.facts.models.iter().filter(|model| model.installed).map(|model| model.id.clone()).collect();
        for detail in self.view.details.clone() {
            ui.add_space(12.0);
            ui.add(Label::new(RichText::new(detail.label.clone()).size(META).color(GREY)));
            ui.add_space(2.0);
            let monospace = MONOSPACE.contains(&detail.label.as_str());
            if detail.label == "Models" {
                for (index, line) in detail.value.split('\n').enumerate() {
                    ui.add_space(4.0);
                    let id = installed.get(index).cloned();
                    self.detail_line(ui, line, monospace, id.as_deref());
                }
            } else {
                self.detail_line(ui, &detail.value, monospace, None);
                let words = match detail.label.as_str() {
                    "Models folder" => Some(format::OPEN_FOLDER_WORDS),
                    "Activity" => Some(format::OPEN_ACTIVITY_WORDS),
                    _ => None,
                };
                if let Some(words) = words {
                    ui.add_space(6.0);
                    let activity = detail.label == "Activity";
                    if self.tertiary(ui, words).clicked() {
                        self.message = reveal(if activity { self.source.activity_path() } else { Some(self.source.root()) });
                    }
                }
            }
        }
        match self.source.busy() {
            Some(Busy::Checking { id }) => {
                ui.add_space(12.0);
                ui.add(Label::new(RichText::new(format!("Checking {}.", format::display_name(&id))).size(META).color(GREY)));
            }
            Some(Busy::Removing { id }) => {
                ui.add_space(12.0);
                ui.add(Label::new(RichText::new(format!("Removing {}.", format::display_name(&id))).size(META).color(GREY)));
            }
            _ => {}
        }
        if self.view.copy_details {
            ui.add_space(12.0);
            if self.tertiary(ui, format::COPY_DETAILS_WORDS).clicked() {
                let report = self.source.bug_report(&self.view);
                ui.ctx().copy_text(report);
            }
        }
    }

    /// One Details value, with `Check again` and `Remove` on its right when it names a model.
    fn detail_line(&mut self, ui: &mut Ui, value: &str, monospace: bool, model: Option<&str>) {
        let text = RichText::new(value.to_string()).size(META).color(INK);
        let width = ui.available_width();
        ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::left_to_right(Align::Center), |ui| {
            ui.add(Label::new(if monospace { text.font(FontId::monospace(META)) } else { text }));
            if let Some(id) = model {
                ui.add_space(12.0);
                if self.tertiary(ui, format::CHECK_AGAIN_WORDS).clicked() {
                    self.press(task::Task::Check { id: id.to_string() });
                }
                ui.add_space(4.0);
                if self.tertiary(ui, format::REMOVE_WORDS).clicked() {
                    self.press(task::Task::Remove { id: id.to_string() });
                }
            }
        });
    }

    /// Start a task, and say whether it started. One that cannot start is a sentence, not a crash
    /// (brief rule 6), and the caller keeps whatever the person had on the screen.
    fn press(&mut self, what: task::Task) -> bool {
        match task::start(&self.source, what) {
            Ok(running) => {
                self.running = Some(running);
                self.message = None;
                true
            }
            Err(fail) => {
                self.message = Some(self.refusal(&fail.sentence));
                false
            }
        }
    }

    /// A refused press in the brief's plain words: the library's sentence, then what this window is
    /// really doing, so nobody has to guess why the button did nothing. A press refused for a reason
    /// the window cannot see - a thread the machine would not start - keeps the library's sentence on
    /// its own rather than inventing work that is not happening.
    fn refusal(&self, why: &str) -> String {
        let doing = match self.source.busy() {
            Some(Busy::Model { id, done, total }) => format!("Downloading {} - {}.", format::display_name(&id), format::of_words(done, total)),
            Some(Busy::Cuda { done, total }) => format!("Downloading the CUDA libraries - {}.", format::of_words(done, total)),
            Some(Busy::Checking { id }) => format!("Checking {}.", format::display_name(&id)),
            Some(Busy::Removing { id }) => format!("Removing {}.", format::display_name(&id)),
            None if self.source.is_claimed() => "This window is already working on something else.".to_string(),
            None => return why.to_string(),
        };
        format!("{why} {doing} Nothing was changed; try again when it finishes.")
    }

    /// The one filled button on the screen. Grey and unclickable until `enabled`, which is how the
    /// brief's two checkboxes gate their buttons.
    fn primary(&mut self, ui: &mut Ui, words: &str, enabled: bool) -> Response {
        let button = Button::new(RichText::new(words).size(BODY).color(if enabled { INK } else { GREY }))
            .fill(if enabled { WHITE } else { GROUND })
            .stroke(Stroke::new(1.0, if enabled { INK } else { RULE }))
            .min_size(Vec2::new(0.0, PRIMARY))
            .corner_radius(RADIUS);
        let response = ui.add(if enabled { button } else { button.sense(Sense::hover()) });
        ring(ui, &response);
        response
    }

    /// An outlined button: `Add \u{b7} 198 MB` on a row, and the offer's `Add`.
    fn secondary(&mut self, ui: &mut Ui, words: &str) -> Response {
        let response = ui.add(
            Button::new(RichText::new(words).size(BODY).color(INK))
                .fill(GROUND)
                .stroke(Stroke::new(1.0, INK))
                .min_size(Vec2::new(0.0, SECONDARY))
                .corner_radius(RADIUS),
        );
        ring(ui, &response);
        response
    }

    /// A word with no frame: Cancel, Not now, Read NVIDIA's terms, Details, and the Details buttons.
    fn tertiary(&mut self, ui: &mut Ui, words: &str) -> Response {
        let response = ui.add(Button::new(RichText::new(words).size(META).color(GREY)).frame(false).min_size(Vec2::new(0.0, TERTIARY)).corner_radius(RADIUS));
        ring(ui, &response);
        response
    }
}

/// The 2 px ink focus ring the brief asks for. egui 0.36 has no focus-ring style to set, so the ring
/// is drawn here from the button's own rectangle.
fn ring(ui: &Ui, response: &Response) {
    if response.has_focus() {
        ui.painter().rect_stroke(response.rect.expand(1.0), RADIUS, Stroke::new(RING, INK), StrokeKind::Middle);
    }
}

/// Open a folder or a file in the shell. The window is Windows-only for now, so this is the only
/// shell it has to know; a machine that will not open it gets a sentence rather than a crash.
fn reveal(path: Option<&Path>) -> Option<String> {
    let Some(path) = path else {
        return Some("There is nothing here to open.".to_string());
    };
    if !cfg!(windows) {
        return Some("This computer has no folder to open.".to_string());
    }
    match std::process::Command::new("explorer.exe").arg(path).spawn() {
        Ok(_) => None,
        Err(_) => Some(format!("Windows would not open {}.", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{ServeOptions, Server};
    use crate::testkit::Scratch;
    use std::ffi::OsString;

    /// The window over a scratch store, with the server that shares the process with it. Every press
    /// under test is refused before it could reach the network, so no test downloads anything.
    fn window_at(root: PathBuf) -> (Server, Window) {
        let server = Server::start(ServeOptions {
            fake_backend: true,
            models_root: Some(root.clone()),
            endpoint_file: Some(root.join("endpoint.json")),
            ..ServeOptions::default()
        })
        .expect("the server starts");
        let source = Arc::new(Source::at_root(server.runner(), server.port(), None, server.routing(), root).expect("the window has a store"));
        (server, Window::new(source, Arc::new(AtomicBool::new(false))))
    }

    /// Say that this window is busy, the way a worker does once it has bytes to report.
    fn make_busy(window: &Window, busy: Busy) {
        let work = window.source.work();
        let mut work = work.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        work.set_busy(Some(busy));
    }

    /// MG-13 review round 5: `WINDIR` names the Windows folder, so the fonts are one level below it.
    /// Before this, a normal Windows machine looked for `segoeui.ttf` in `C:\Windows` and the window
    /// refused to open at all.
    #[test]
    fn windir_names_the_windows_folder_so_the_fonts_are_one_level_below_it() {
        assert_eq!(fonts_folder_from(Some(OsString::from("C:/Windows"))), PathBuf::from("C:/Windows/Fonts"));
        assert_eq!(fonts_folder_from(None), PathBuf::from(r"C:\Windows\Fonts"));
    }

    /// The same machine seen the way Windows writes its own variable, and the face it draws with.
    #[cfg(windows)]
    #[test]
    fn a_normal_windows_machine_has_the_fonts_the_window_draws_with() {
        assert_eq!(fonts_folder_from(Some(OsString::from(r"C:\Windows"))), PathBuf::from(r"C:\Windows\Fonts"));
        let fonts = system_fonts().expect("this machine has the fonts Windows itself uses");
        assert!(fonts.font_data.contains_key("segoeui"), "the window's own face comes from the system folder");
    }

    /// MG-13 review round 5: a refused Download used to close the licence note and clear the box the
    /// person had just ticked, so the only way to retry was to read the licence all over again.
    #[test]
    fn a_refused_download_keeps_the_licence_note_open_and_ticked() {
        let scratch = Scratch::new("window-refused-download");
        let (_server, mut window) = window_at(scratch.path.clone());
        window.open_note = Some("birefnet-lite".to_string());
        window.note_ticked = true;
        make_busy(&window, Busy::Model { id: "birefnet-lite".to_string(), done: 80 * 1024 * 1024, total: 198 * 1024 * 1024 });
        window.download_pressed("birefnet-lite");
        assert_eq!(window.open_note.as_deref(), Some("birefnet-lite"), "the note is still open under its row");
        assert!(window.note_ticked, "and the box is still ticked, so the retry is one press away");
        assert!(window.running.is_none(), "nothing started");
        let message = window.message.clone().expect("the window says why the press was refused");
        assert!(message.contains("Something else is happening in this window right now."), "the library's own sentence: {message}");
        assert!(message.contains("Downloading BiRefNet lite - 80 of 198 MB."), "and what is really happening: {message}");
        assert!(message.contains("Nothing was changed; try again when it finishes."), "and that nothing was lost: {message}");
    }

    /// The offer's terms are the same shape, so they carry the same rule.
    #[test]
    fn a_refused_cuda_download_keeps_the_terms_open_and_ticked() {
        let scratch = Scratch::new("window-refused-cuda");
        let (_server, mut window) = window_at(scratch.path.clone());
        window.cuda_open = true;
        window.cuda_ticked = true;
        make_busy(&window, Busy::Cuda { done: 512 * 1024 * 1024, total: 1024 * 1024 * 1024 });
        window.cuda_download_pressed();
        assert!(window.cuda_open, "the terms are still on the screen");
        assert!(window.cuda_ticked, "and the acceptance the person gave is still given");
        assert!(window.running.is_none(), "nothing started");
        let message = window.message.clone().expect("the window says why the press was refused");
        assert!(message.contains("Downloading the CUDA libraries - 0.5 of 1.0 GB."), "the progress the worker reported: {message}");
    }

    /// A worker that has taken the window but has no bytes yet has nothing to report, so the window
    /// says that rather than a percentage it does not have.
    #[test]
    fn a_refused_press_says_the_window_is_busy_before_any_worker_has_reported() {
        let scratch = Scratch::new("window-refused-claim");
        let (_server, mut window) = window_at(scratch.path.clone());
        window.open_note = Some("lama".to_string());
        window.note_ticked = true;
        let work = window.source.work();
        assert!(work.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).claim(), "the window takes its own worker slot");
        window.download_pressed("lama");
        assert_eq!(window.open_note.as_deref(), Some("lama"), "the note stays open");
        assert!(window.note_ticked, "and its box stays ticked");
        let message = window.message.clone().expect("the window says why the press was refused");
        assert!(message.contains("This window is already working on something else."), "no invented progress: {message}");
    }

    /// The other half: a press that is accepted still clears its box and starts its worker. This one
    /// uses `Acknowledge`, which touches the store and never the network.
    #[test]
    fn an_accepted_press_starts_its_worker_and_clears_its_box() {
        let scratch = Scratch::new("window-accepted-press");
        let (_server, mut window) = window_at(scratch.path.clone());
        window.action_ticked = true;
        window.action_pressed(Some("lama"));
        assert!(!window.action_ticked, "the box clears once the work has started");
        assert!(window.message.is_none(), "an accepted press says nothing");
        let mut running = window.running.take().expect("the worker started");
        running.join();
    }
}
