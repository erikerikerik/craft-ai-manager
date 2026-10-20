//! The words of the window: every string the brief fixes, computed from a number.
//!
//! Nothing in here reads a file, a socket or the clock. The state model in [crate::window::state]
//! hands over integers and gets back a sentence, which is what makes the tables of
//! `agents/CAIM-UI-BRIEF.md` sections 3 and 4 testable.

/// The catalog task behind Generative Fill. `routing` names the mask task; the window needs the
/// fill task by name too, which is the one the catalog calls `fill_masked_area`.
pub const TASK_FILL: &str = "fill_masked_area";

/// The units [size] counts in: 1024-based, the way Explorer does.
const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// Bytes as Explorer shows them: 1024-based, labelled KB, MB or GB.
///
/// The brief's rule is "one decimal under 10 GB, none above 10 MB", with three examples:
/// 208,044,816 = 198 MB, 224,005,088 = 214 MB and 1,915,544,656 = 1.8 GB. So a size in megabytes
/// is a whole number and a size in gigabytes carries one decimal until it reaches ten of them.
#[must_use]
pub fn size(bytes: u64) -> String {
    let (number, unit) = size_parts(bytes);
    format!("{number} {unit}")
}

/// [size] split in two, so a sentence can say "120 of 198 MB" without repeating the unit.
#[must_use]
pub fn size_parts(bytes: u64) -> (String, &'static str) {
    let (divisor, unit) = scale(bytes);
    (in_unit(bytes, divisor), unit)
}

/// The unit [size_parts] would choose for this many bytes, as a divisor and a label.
fn scale(bytes: u64) -> (u64, &'static str) {
    if bytes >= GIB {
        (GIB, "GB")
    } else if bytes >= MIB {
        (MIB, "MB")
    } else if bytes >= KIB {
        (KIB, "KB")
    } else {
        (1, "bytes")
    }
}

/// A byte count measured in a unit chosen somewhere else, which is what lets one sentence say
/// "0.7 of 1.8 GB" rather than "668 MB of 1.8 GB".
fn in_unit(bytes: u64, divisor: u64) -> String {
    if divisor == GIB {
        // Rounded to tenths of a GB, and to whole GB once the size reaches ten of them.
        let tenths = bytes.saturating_mul(10).saturating_add(divisor / 2) / divisor;
        if tenths >= 100 { format!("{}", tenths / 10) } else { format!("{}.{}", tenths / 10, tenths % 10) }
    } else if divisor == 1 {
        format!("{bytes}")
    } else {
        format!("{}", bytes.saturating_add(divisor / 2) / divisor)
    }
}

/// Memory as the Details list shows it: 22,118 MiB free reads "21.6" next to a "24 GB" card.
///
/// The brief gives the free figure one decimal and the total as a plain [size], which is why the
/// two are separate functions rather than one call with a flag.
#[must_use]
pub fn gib_words(mebibytes: u64) -> String {
    let tenths = mebibytes.saturating_mul(10).saturating_add(512) / 1024;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// A count with the thousands separators the brief uses: 1000 reads "1,000".
#[must_use]
pub fn group(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// How far along a download is, as a whole percentage: 120 of 198 MB is 61%.
#[must_use]
pub fn percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    done.saturating_mul(100).saturating_add(total / 2).saturating_div(total).min(100)
}

/// How long ago something finished, in the brief's words: "just now", "2 s ago", "4 min ago",
/// "3 h ago", "yesterday", "4 days ago". Zero seconds is "just now", which is what lets the
/// running line read "Started 2 s ago" using the same function.
#[must_use]
pub fn ago_words(seconds: u64) -> String {
    match seconds {
        0 => "just now".to_string(),
        1..=59 => format!("{seconds} s ago"),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        86_400..=172_799 => "yesterday".to_string(),
        other => format!("{} days ago", other / 86_400),
    }
}

/// How long this run has been up: "3 h 12 min", "12 min", "40 s".
#[must_use]
pub fn uptime_words(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    if hours > 0 {
        if minutes > 0 { format!("{hours} h {minutes} min") } else { format!("{hours} h") }
    } else if seconds >= 60 {
        format!("{minutes} min")
    } else {
        format!("{seconds} s")
    }
}

/// How long a job took, as the activity line wants it: one decimal under 10 s, whole seconds
/// above it. 300 ms is "0.3 s"; 12,400 ms is "12 s".
#[must_use]
pub fn elapsed_words(millis: u64) -> String {
    if millis < 10_000 {
        let tenths = millis.saturating_add(50) / 100;
        format!("{}.{} s", tenths / 10, tenths % 10)
    } else {
        format!("{} s", millis.saturating_add(500) / 1000)
    }
}

/// How long a job usually takes, for "usually about 5 s on the processor". The brief's own
/// example is a whole number of seconds, so the median is rounded rather than shown to a decimal.
#[must_use]
pub fn about_words(millis: u64) -> String {
    format!("{} s", millis.saturating_add(500) / 1000)
}

/// The device a job ran on, in words. `cpu` and `cuda` are the two the manager reports; anything
/// else says "this computer" rather than guessing at hardware the brief does not name.
#[must_use]
pub fn device_words(device: &str) -> &'static str {
    match device {
        "cpu" => "on the processor",
        "cuda" => "on the graphics card",
        _ => "on this computer",
    }
}

/// Why a job left the device it asked for, in the brief's five sentences. A reason the brief does
/// not list has no words, and the line then says nothing about it.
#[must_use]
pub fn fallback_words(reason: &str) -> Option<&'static str> {
    match reason {
        "backend_unavailable" => Some("the graphics card libraries aren't installed"),
        "backend_init_failed" => Some("the graphics card wouldn't start"),
        "insufficient_memory" => Some("the graphics card didn't have enough free memory"),
        "unsupported_model" => Some("this model isn't cleared for the graphics card yet"),
        "execution_failed" => Some("the graphics card failed partway, so it ran again on the processor"),
        _ => None,
    }
}

/// What one catalog model does for PhotoCraft: the row title, the purpose line, and how many
/// PhotoCraft features it counts towards the status sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feature {
    /// The row title, which is PhotoCraft's name for the feature.
    pub title: &'static str,
    /// The one-line purpose under the title.
    pub purpose: &'static str,
    /// How many features this model enables, which is what the status sentence counts.
    pub count: usize,
}

/// The feature a catalog task belongs to. The brief keys the titles by task rather than by model
/// id, so a second model for the same task would carry the same words.
#[must_use]
pub fn feature(task: &str) -> Option<Feature> {
    match task {
        TASK_FILL => Some(Feature { title: "Generative Fill", purpose: "Removes what you paint over and fills the gap.", count: 1 }),
        crate::routing::TASK_MASK => {
            Some(Feature { title: "Select Subject \u{b7} Remove Background", purpose: "Finds the main subject, to select it or mask out the rest.", count: 2 })
        }
        _ => None,
    }
}

/// The short name of a task, for the activity line's "Last: Select Subject, 4 min ago". The brief
/// uses the first part of the row title there, not the whole title.
#[must_use]
pub fn task_words(task: &str) -> &'static str {
    match task {
        TASK_FILL => "Generative Fill",
        crate::routing::TASK_MASK => "Select Subject",
        _ => "an AI feature",
    }
}

/// What the window says while a job is running: "Filling the masked area for PhotoCraft."
#[must_use]
pub fn running_words(task: &str) -> &'static str {
    match task {
        TASK_FILL => "Filling the masked area for PhotoCraft.",
        crate::routing::TASK_MASK => "Selecting the subject for PhotoCraft.",
        _ => "Working for PhotoCraft.",
    }
}

/// The model's name as the brief writes it. The catalog keeps ids lower-case; the window does not
/// shout them back.
#[must_use]
pub fn display_name(id: &str) -> String {
    match id {
        "lama" => "LaMa".to_string(),
        "birefnet-lite" => "BiRefNet lite".to_string(),
        other => other.to_string(),
    }
}

/// A date as the Details list shows it: "Oct 13". The calendar is the same `civil_from_days` the
/// activity log stamps with, so the two can never disagree.
#[must_use]
pub fn date_words(unix: u64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (_, month, day) = crate::activity::civil_from_days(i64::try_from(unix / 86_400).unwrap_or(i64::MAX));
    let index = usize::try_from(month.saturating_sub(1)).unwrap_or(0);
    let name = MONTHS.get(index).copied().unwrap_or("Jan");
    format!("{name} {day}")
}

// The words the brief fixes, kept here so the drawing layer never carries copy of its own.

/// The window's own name, which the title bar and the bug report both use.
pub const WINDOW_TITLE: &str = "Craft AI Manager";
/// Section 4.1: the heading over a model's licence note.
pub const NOTE_HEADING: &str = "Non-commercial use only";
/// Section 4.1: the checkbox that acknowledges a licence note.
pub const ACKNOWLEDGE_WORDS: &str = "I understand it's for non-commercial use only.";
/// Section 4.3: the sentence over NVIDIA's terms.
pub const CUDA_TERMS: &str =
    "NVIDIA allows these libraries to be passed on, but they aren't open source. Without them every feature still works, on the processor.";
/// Section 4.3: the checkbox that accepts NVIDIA's licence.
pub const CUDA_ACCEPT_WORDS: &str = "I accept NVIDIA's licence terms.";
/// Section 4.3: the control that shows the two notes from [crate::cuda].
pub const CUDA_LINK_WORDS: &str = "Read NVIDIA's terms";
/// Section 4.1: the tail of a model's download line.
pub const MODEL_DOWNLOADING_SUFFIX: &str = "checked as it arrives";
/// Section 4.3: the tail of the CUDA libraries download line.
pub const CUDA_DOWNLOADING_SUFFIX: &str = "features keep running on the processor meanwhile";
/// The tertiary button that turns an offer or a licence note away.
pub const NOT_NOW: &str = "Not now";
/// The tertiary button that stops a download and keeps the part already here.
pub const CANCEL: &str = "Cancel";
/// Rule 4: the one question asked when the window closes over a running job.
pub const CLOSE_QUESTION: &str = "A job is running for PhotoCraft. Close anyway?";
/// Rule 4: the two answers to [CLOSE_QUESTION].
pub const CLOSE_WORDS: &str = "Close";
pub const KEEP_OPEN_WORDS: &str = "Keep open";
/// Section 3 item 5: the footer's diagnostics toggle. The brief fixes the closed word; the open
/// word is the same control read back, which the brief does not give (see the MG-13 receipt).
pub const DETAILS_WORDS: &str = "Details";
pub const HIDE_DETAILS_WORDS: &str = "Hide details";
/// Section 4.5: the four tertiary controls inside the Details list.
pub const CHECK_AGAIN_WORDS: &str = "Check again";
pub const REMOVE_WORDS: &str = "Remove";
pub const OPEN_FOLDER_WORDS: &str = "Open folder";
pub const OPEN_ACTIVITY_WORDS: &str = "Open activity log";
/// Section 4.5: the copy button, offered only on a needs-you state.
pub const COPY_DETAILS_WORDS: &str = "Copy details for a bug report";
/// Erik's acceptance review (Oct 20, 2026): the device statement, which the main column now says in
/// its own words instead of leaving a bare hardware name in the footer. The label is the point - a
/// reader has to know they are being told what runs the AI, not what machine this is.
pub const DEVICE_LABEL: &str = "AI device";
/// The processor, in the same plain words the brief's Details line uses.
pub const DEVICE_CPU: &str = "Processor (CPU)";

/// The primary button of a licence note: "Download \u{b7} 198 MB".
#[must_use]
pub fn download_button(bytes: u64) -> String {
    format!("Download \u{b7} {}", size(bytes))
}

/// How far a download has got, in the total's unit: "93 of 198 MB".
///
/// Both numbers are measured in the total's unit, which is what the brief's "0.7 of 1.8 GB" needs.
#[must_use]
pub fn of_words(done: u64, total: u64) -> String {
    let (divisor, unit) = scale(total);
    format!("{} of {} {unit}", in_unit(done, divisor), in_unit(total, divisor))
}

/// The line under a download in flight: "Downloading 93 of 198 MB \u{b7} checked as it arrives".
///
/// The suffix is the only difference between a model's line and the CUDA one.
#[must_use]
pub fn downloading_line(done: u64, total: u64, suffix: &str) -> String {
    format!("Downloading {} \u{b7} {suffix}", of_words(done, total))
}

/// The footer without its own toggle, so the toggle can be drawn as a control beside it.
#[must_use]
pub fn footer_without_details(footer: &str) -> &str {
    footer.strip_suffix(" \u{b7} Details").unwrap_or(footer)
}

/// The card as the window names it: the probe's own name in brackets, so the person can see which
/// hardware the manager found.
#[must_use]
pub fn device_gpu(name: &str) -> String {
    format!("Graphics card ({name})")
}

/// The whole device line: `AI device: Graphics card (NVIDIA GeForce RTX 4090)` when this run would
/// use the card, and `AI device: Processor (CPU)` when it would not.
///
/// `None` covers every case where the card is not the answer - no card, a card whose NVIDIA
/// libraries are not installed, and a run started with `--device cpu`. The state model works that
/// out; this function only says it.
#[must_use]
pub fn device_line(gpu_name: Option<&str>) -> String {
    format!("{DEVICE_LABEL}: {}", gpu_name.map_or_else(|| DEVICE_CPU.to_string(), device_gpu))
}
#[cfg(test)]
mod tests {
    use super::{
        ACKNOWLEDGE_WORDS, CANCEL, CHECK_AGAIN_WORDS, CLOSE_QUESTION, CLOSE_WORDS, COPY_DETAILS_WORDS, CUDA_ACCEPT_WORDS, CUDA_DOWNLOADING_SUFFIX,
        CUDA_LINK_WORDS, CUDA_TERMS, DETAILS_WORDS, DEVICE_CPU, DEVICE_LABEL, HIDE_DETAILS_WORDS, KEEP_OPEN_WORDS, MODEL_DOWNLOADING_SUFFIX, NOT_NOW,
        NOTE_HEADING, OPEN_ACTIVITY_WORDS, OPEN_FOLDER_WORDS, REMOVE_WORDS, WINDOW_TITLE, about_words, ago_words, date_words, device_gpu, device_line,
        device_words, display_name, download_button, downloading_line, elapsed_words, fallback_words, feature, footer_without_details, gib_words, group,
        of_words, percent, running_words, size, size_parts, task_words, uptime_words,
    };

    /// Section 3 of the brief, exactly: the three sizes it names and the rule behind them.
    #[test]
    fn sizes_use_windows_units_and_the_briefs_three_examples() {
        assert_eq!(size(208_044_816), "198 MB");
        assert_eq!(size(224_005_088), "214 MB");
        assert_eq!(size(1_915_544_656), "1.8 GB");
        // One decimal under 10 GB, none above 10 MB.
        assert_eq!(size(9_999_999_999), "9.3 GB");
        assert_eq!(size(12 * 1024 * 1024 * 1024), "12 GB");
        // Below a megabyte the unit follows Explorer, because the brief names none.
        assert_eq!(size(1024), "1 KB");
        assert_eq!(size(0), "0 bytes");
        assert_eq!(size_parts(120 * 1024 * 1024), ("120".to_string(), "MB"));
    }

    /// The two numbers of a download share the total's unit, wherever a sentence quotes them.
    #[test]
    fn the_two_numbers_of_a_download_share_the_totals_unit() {
        assert_eq!(of_words(97_517_568, 208_044_816), "93 of 198 MB");
        assert_eq!(of_words(751_619_276, 1_915_544_656), "0.7 of 1.8 GB");
        assert_eq!(of_words(0, 208_044_816), "0 of 198 MB");
    }

    #[test]
    fn a_part_of_a_download_is_a_whole_percentage() {
        assert_eq!(percent(120 * 1024 * 1024, 198 * 1024 * 1024), 61);
        assert_eq!(percent(0, 198 * 1024 * 1024), 0);
        assert_eq!(percent(198 * 1024 * 1024, 198 * 1024 * 1024), 100);
        // A part longer than the catalog says it is cannot show 101%.
        assert_eq!(percent(400 * 1024 * 1024, 198 * 1024 * 1024), 100);
        assert_eq!(percent(10, 0), 0);
    }

    #[test]
    fn time_ago_reads_the_way_the_brief_writes_it() {
        assert_eq!(ago_words(0), "just now");
        assert_eq!(ago_words(2), "2 s ago");
        assert_eq!(ago_words(59), "59 s ago");
        assert_eq!(ago_words(240), "4 min ago");
        assert_eq!(ago_words(11_000), "3 h ago");
        assert_eq!(ago_words(86_400), "yesterday");
        assert_eq!(ago_words(4 * 86_400), "4 days ago");
    }

    #[test]
    fn uptimes_and_elapsed_times_are_the_briefs_two_shapes() {
        assert_eq!(uptime_words(11_520), "3 h 12 min");
        assert_eq!(uptime_words(3_600), "1 h");
        assert_eq!(uptime_words(720), "12 min");
        assert_eq!(uptime_words(40), "40 s");
        assert_eq!(elapsed_words(300), "0.3 s");
        assert_eq!(elapsed_words(5_000), "5.0 s");
        assert_eq!(elapsed_words(12_400), "12 s");
        assert_eq!(about_words(4_500), "5 s");
    }

    #[test]
    fn devices_and_fallback_reasons_have_the_words_the_table_gives() {
        assert_eq!(device_words("cpu"), "on the processor");
        assert_eq!(device_words("cuda"), "on the graphics card");
        assert_eq!(device_words("coreml"), "on this computer");
        assert_eq!(fallback_words("backend_unavailable"), Some("the graphics card libraries aren't installed"));
        assert_eq!(fallback_words("backend_init_failed"), Some("the graphics card wouldn't start"));
        assert_eq!(fallback_words("insufficient_memory"), Some("the graphics card didn't have enough free memory"));
        assert_eq!(fallback_words("unsupported_model"), Some("this model isn't cleared for the graphics card yet"));
        assert_eq!(fallback_words("execution_failed"), Some("the graphics card failed partway, so it ran again on the processor"));
        assert_eq!(fallback_words("something_new"), None);
    }

    #[test]
    fn the_two_catalog_tasks_map_to_the_briefs_titles_purposes_and_counts() {
        let fill = feature("fill_masked_area").expect("the fill task has a feature");
        assert_eq!(fill.title, "Generative Fill");
        assert_eq!(fill.purpose, "Removes what you paint over and fills the gap.");
        assert_eq!(fill.count, 1);
        let mask = feature("subject_mask").expect("the mask task has a feature");
        assert_eq!(mask.title, "Select Subject \u{b7} Remove Background");
        assert_eq!(mask.purpose, "Finds the main subject, to select it or mask out the rest.");
        assert_eq!(mask.count, 2);
        assert_eq!(feature("some_future_task"), None);
        assert_eq!(task_words("subject_mask"), "Select Subject");
        assert_eq!(running_words("fill_masked_area"), "Filling the masked area for PhotoCraft.");
        assert_eq!(running_words("subject_mask"), "Selecting the subject for PhotoCraft.");
    }

    #[test]
    fn model_names_are_written_the_way_the_brief_writes_them() {
        assert_eq!(display_name("lama"), "LaMa");
        assert_eq!(display_name("birefnet-lite"), "BiRefNet lite");
        assert_eq!(display_name("something-new"), "something-new");
    }

    #[test]
    fn a_date_is_the_month_and_the_day() {
        // 2025-10-13, the first day of a year, and the last day of a leap February.
        assert_eq!(date_words(1_760_320_800), "Oct 13");
        assert_eq!(date_words(1_735_689_600), "Jan 1");
        assert_eq!(date_words(951_782_400), "Feb 29");
    }

    /// Section 4.5: the card line reads "21.6 of 24 GB free", the activity line "1,000".
    #[test]
    fn memory_and_counts_are_written_the_way_details_writes_them() {
        assert_eq!(gib_words(22_118), "21.6");
        assert_eq!(gib_words(24_576), "24.0");
        assert_eq!(gib_words(0), "0.0");
        assert_eq!(group(1_000), "1,000");
        assert_eq!(group(999), "999");
        assert_eq!(group(1_915_544), "1,915,544");
        assert_eq!(group(0), "0");
    }

    /// The brief's fixed copy lives here, so the drawing layer never carries words of its own.
    #[test]
    fn the_briefs_fixed_words_are_written_here() {
        assert_eq!(WINDOW_TITLE, "Craft AI Manager");
        assert_eq!(NOTE_HEADING, "Non-commercial use only");
        assert_eq!(ACKNOWLEDGE_WORDS, "I understand it's for non-commercial use only.");
        assert_eq!(
            CUDA_TERMS,
            "NVIDIA allows these libraries to be passed on, but they aren't open source. Without them every feature still works, on the processor."
        );
        assert_eq!(CUDA_ACCEPT_WORDS, "I accept NVIDIA's licence terms.");
        assert_eq!(CUDA_LINK_WORDS, "Read NVIDIA's terms");
        assert_eq!(NOT_NOW, "Not now");
        assert_eq!(CANCEL, "Cancel");
        assert_eq!(CLOSE_QUESTION, "A job is running for PhotoCraft. Close anyway?");
        assert_eq!((CLOSE_WORDS, KEEP_OPEN_WORDS), ("Close", "Keep open"));
        assert_eq!((DETAILS_WORDS, HIDE_DETAILS_WORDS), ("Details", "Hide details"));
        assert_eq!((CHECK_AGAIN_WORDS, REMOVE_WORDS), ("Check again", "Remove"));
        assert_eq!((OPEN_FOLDER_WORDS, OPEN_ACTIVITY_WORDS), ("Open folder", "Open activity log"));
        assert_eq!(COPY_DETAILS_WORDS, "Copy details for a bug report");
    }

    /// Section 4.1 and 4.3: the two download buttons and the two download lines.
    #[test]
    fn a_download_button_and_a_download_line_use_the_briefs_sizes() {
        assert_eq!(download_button(208_044_816), "Download \u{b7} 198 MB");
        assert_eq!(download_button(224_005_088), "Download \u{b7} 214 MB");
        assert_eq!(download_button(1_915_544_656), "Download \u{b7} 1.8 GB");
        assert_eq!(downloading_line(97_517_568, 208_044_816, MODEL_DOWNLOADING_SUFFIX), "Downloading 93 of 198 MB \u{b7} checked as it arrives");
        assert_eq!(
            downloading_line(751_619_276, 1_915_544_656, CUDA_DOWNLOADING_SUFFIX),
            "Downloading 0.7 of 1.8 GB \u{b7} features keep running on the processor meanwhile"
        );
        assert_eq!(
            downloading_line(0, 1_915_544_656, CUDA_DOWNLOADING_SUFFIX),
            "Downloading 0.0 of 1.8 GB \u{b7} features keep running on the processor meanwhile"
        );
        assert_eq!(downloading_line(120 * 1024 * 1024, 198 * 1024 * 1024, MODEL_DOWNLOADING_SUFFIX), "Downloading 120 of 198 MB \u{b7} checked as it arrives");
    }

    /// Section 3 item 5: the footer's own word is drawn as a control, not as part of the sentence.
    #[test]
    fn the_footer_keeps_its_words_when_the_toggle_is_drawn_beside_it() {
        let footer = "This computer only \u{b7} Closing this window turns AI features off \u{b7} Details";
        assert_eq!(footer_without_details(footer), "This computer only \u{b7} Closing this window turns AI features off");
        assert_eq!(footer_without_details("a footer with no toggle"), "a footer with no toggle");
    }

    /// Erik's acceptance review, finding 2: the device is said in a labeled sentence, in the main
    /// column, and the two answers are the person's words for the hardware rather than a driver name.
    #[test]
    fn the_device_line_says_which_hardware_runs_the_ai() {
        assert_eq!(device_line(None), "AI device: Processor (CPU)");
        assert_eq!(device_line(Some("NVIDIA GeForce RTX 4090")), "AI device: Graphics card (NVIDIA GeForce RTX 4090)");
        assert_eq!(device_gpu("NVIDIA GeForce RTX 4090"), "Graphics card (NVIDIA GeForce RTX 4090)");
        assert_eq!((DEVICE_LABEL, DEVICE_CPU), ("AI device", "Processor (CPU)"));
    }
}
