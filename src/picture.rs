//! Images a tool call looked at, drawn in the terminal.
//!
//! An image is the one tool result that is not text, and it reaches the row two ways. A
//! subagent's transcript is the provider's own file and carries the picture inside it. A
//! thread's own rows carry only the path the tool read, because the server projects tool
//! results down to a summary before they go on the wire and an image does not survive
//! that — so the file is read from the disk it names, which is this machine when the
//! server runs here, and nothing at all when it runs somewhere else.
//!
//! Terminals disagree about how a picture is drawn, so the terminal is asked once what it
//! speaks — kitty, iTerm2 or sixel — and where it speaks none of them the image is drawn
//! in half-blocks, which is coarse but is still the image.
//!
//! Decoding and encoding are the expensive part and happen once per image and size, when
//! the row is opened. Everything after that is placement, so scrolling past an open image
//! costs no more than scrolling past text. A large image is decoded and encoded on a
//! thread of its own: how much room it takes is worked out from its header alone, so the
//! rows are kept for it at once and it is drawn into them when it is ready, rather than
//! the screen standing still while it is made.

use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::Engine;
use ratatui::{
    Frame,
    layout::{Rect, Size},
};
use ratatui_image::{
    FontSize, Resize,
    picker::{Picker, cap_parser::QueryStdioOptions},
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

/// How many images are kept ready to draw. Each holds the encoded form the terminal was
/// given, which for a picture in the chat is large, so this is not a cache of everything
/// a thread ever showed — it is enough that the rows anyone has open stay ready, and that
/// a list of projects drawn with their icons does not push them all out. An image that
/// falls out of it is encoded again the next time it is drawn, which costs a few
/// milliseconds.
const KEPT: usize = 48;

struct Ready {
    key: String,
    /// The room the row offered when this was encoded. A narrower window is a different
    /// image as far as the terminal is concerned, so it is encoded again.
    offered: Size,
    size: Size,
    protocol: SlicedProtocol,
}

/// An image being made on a thread of its own, and the room it will take when it is.
struct Pending {
    key: String,
    offered: Size,
    size: Size,
}

/// What a thread made of an image: ready to draw, or `None` where the bytes were not an
/// image after all.
struct Made {
    key: String,
    offered: Size,
    protocol: Option<SlicedProtocol>,
}

/// Where the threads leave what they made. Each store has its own, and a store that
/// starts over takes a new one, so an image begun before that is not taken in after it.
type Inbox = Arc<Mutex<Vec<Made>>>;

#[derive(Default)]
struct Store {
    /// What the terminal answered. `None` until asked, and after an answer that left no
    /// way to draw anything.
    picker: Option<Picker>,
    /// Whether a path the server names is a path here. False against a server on another
    /// machine, where the same path is either nothing or, worse, another file entirely.
    local_files: bool,
    ready: VecDeque<Ready>,
    pending: Vec<Pending>,
    /// Images a thread found it could not make, so they are not tried on every frame.
    failed: Vec<(String, Size)>,
    inbox: Inbox,
}

impl Store {
    /// Take in what the threads have made since the last look.
    fn collect(&mut self) {
        let made = std::mem::take(&mut *self.inbox.lock().unwrap());
        for made in made {
            let Some(index) = self
                .pending
                .iter()
                .position(|pending| pending.key == made.key && pending.offered == made.offered)
            else {
                continue;
            };
            let pending = self.pending.swap_remove(index);
            match made.protocol {
                Some(protocol) => {
                    if protocol.size() != pending.size {
                        tracing::debug!(
                            "{} came out {:?} where {:?} was kept for it",
                            made.key,
                            protocol.size(),
                            pending.size
                        );
                    }
                    self.keep(&made.key, made.offered, protocol);
                }
                None => self.failed.push((made.key, made.offered)),
            }
        }
    }

    /// Keep an encoded image ready to draw, as the newest.
    fn keep(&mut self, key: &str, offered: Size, protocol: SlicedProtocol) -> Size {
        let size = protocol.size();
        // Only this size is replaced. The same icon is drawn at one size in the sidebar and
        // another in the picker, and dropping the other size here would have each re-encode
        // the other's on every frame — a new image for the terminal each time, and a flicker.
        self.ready
            .retain(|ready| ready.key != key || ready.offered != offered);
        self.ready.push_front(Ready {
            key: key.to_string(),
            offered,
            size,
            protocol,
        });
        self.ready.truncate(KEPT);
        size
    }

    /// Drop everything made and being made, for a start over.
    fn reset(&mut self) {
        self.ready.clear();
        self.pending.clear();
        self.failed.clear();
        self.inbox = Inbox::default();
    }
}

/// Wakes whoever draws when a thread has made an image.
static MADE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Wait until a thread has made an image, which is then worth a new frame.
pub async fn made() {
    MADE.notified().await;
}

/// Where an image's bytes are.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    /// Base64, as the provider wrote it into the result.
    Data(&'a str),
    /// A path on the machine that ran the tool.
    File(&'a str),
    /// The bytes themselves, for an image that was fetched rather than read.
    Bytes(&'a [u8]),
    /// Pixels as a program handed them over, with no container around them to say how
    /// big they are or how many channels they have.
    Pixels {
        bytes: &'a [u8],
        width: u32,
        height: u32,
        alpha: bool,
    },
}

/// A file this large is not a screenshot and is not worth the memory of finding out.
const MOST_BYTES: u64 = 32 * 1024 * 1024;

/// An image with no more pixels than this is made where it is asked for. Icons and
/// thumbnails take less time to make than a thread takes to hand them back, and an icon
/// that turned up a frame late would blink.
const MADE_HERE: u64 = 512 * 512;

thread_local! {
    static STORE: RefCell<Store> = RefCell::new(Store::default());
}

/// Ask the terminal what it can draw, and say whether the paths the server names are
/// paths here. Called once, after the screen is taken over and before events are read:
/// the answer comes back on stdin, and the event reader would take it for typing.
pub fn ask_terminal(local_files: bool) {
    let picker = Picker::from_query_stdio_with_options(QueryStdioOptions {
        // A terminal answers a question about itself in the time it takes to get there
        // and back. One that says nothing at all is not going to, and waiting on it is
        // an empty screen for as long as we wait.
        timeout: Duration::from_millis(500),
        // Kitty is otherwise sent raw pixels, which for a picture as wide as the chat is
        // over ten megabytes of base64 written in one frame, and the screen waits while
        // the terminal reads it all. The pictures here are mostly screenshots, which
        // deflate to a twentieth of that in a few milliseconds.
        kitty_compression: true,
        ..QueryStdioOptions::default()
    });
    match &picker {
        // Which way images will be drawn is the first thing to know when they come out
        // wrong, and it is decided before there is anywhere on the screen to say it.
        Ok(picker) => tracing::info!(
            "drawing images as {:?} at {:?}",
            picker.protocol_type(),
            picker.font_size()
        ),
        Err(err) => tracing::warn!("no image support: {err}"),
    }
    STORE.with(|store| {
        let mut store = store.borrow_mut();
        store.picker = picker.ok();
        store.local_files = local_files;
    });
}

/// How many pixels a character cell is, where the terminal has said. A picture made for
/// a given number of cells is made at their size rather than near it.
pub fn cell_size() -> Option<Size> {
    STORE.with(|store| {
        let font = store.borrow().picker.as_ref()?.font_size();
        Some(Size::new(font.width, font.height))
    })
}

/// Whether an image named by a path can be shown at all, which is what tells a row with
/// nothing but a path whether it has anything to unfold.
pub fn reads_files() -> bool {
    STORE.with(|store| store.borrow().local_files)
}

pub fn set_local_files(local: bool) {
    STORE.with(|store| {
        let mut store = store.borrow_mut();
        store.local_files = local;
        store.reset();
    });
}

/// Forget what has been drawn. The screen has been handed to another program and back,
/// and whatever the terminal was holding went with it.
pub fn forget() {
    STORE.with(|store| store.borrow_mut().ready.clear());
}

/// Take the image in, and say how much room it wants within `offered`. `None` when it
/// cannot be drawn: a file that is not there or not ours to read, an encoding this build
/// does not read, bytes that are not an image, or a terminal with no way to draw one.
/// A large image whose header reads but whose pixels do not is only found out once its
/// thread has tried, and answers `None` from then on.
pub fn place(key: &str, source: Source<'_>, offered: Size) -> Option<Size> {
    if offered.width == 0 || offered.height == 0 {
        return None;
    }
    STORE.with(|store| {
        let mut store = store.borrow_mut();
        store.collect();
        if let Some(index) = store
            .ready
            .iter()
            .position(|ready| ready.key == key && ready.offered == offered)
        {
            // Draw it again, and move it to the front so the rows in view are the ones
            // that survive.
            let ready = store.ready.remove(index)?;
            let size = ready.size;
            store.ready.push_front(ready);
            return Some(size);
        }
        if let Some(pending) = store
            .pending
            .iter()
            .find(|pending| pending.key == key && pending.offered == offered)
        {
            return Some(pending.size);
        }
        if store
            .failed
            .iter()
            .any(|(failed, at)| failed == key && *at == offered)
        {
            return None;
        }
        // Nothing can be drawn at all until the terminal has said how.
        let picker = store.picker.clone()?;
        let input = Input::of(source, store.local_files)?;
        let (width, height) = input.dimensions()?;
        if u64::from(width) * u64::from(height) <= MADE_HERE {
            let protocol = make(&picker, input.decode()?, offered)?;
            return Some(store.keep(key, offered, protocol));
        }
        let size = fitted(width, height, picker.font_size(), offered);
        store.pending.push(Pending {
            key: key.to_string(),
            offered,
            size,
        });
        let inbox = store.inbox.clone();
        let key = key.to_string();
        std::thread::spawn(move || {
            let protocol = input
                .decode()
                .and_then(|image| make(&picker, image, offered));
            inbox.lock().unwrap().push(Made {
                key,
                offered,
                protocol,
            });
            MADE.notify_one();
        });
        Some(size)
    })
}

/// An image's bytes, held for as long as it takes to make it.
enum Input {
    Encoded(Vec<u8>),
    /// A file the size of which has been checked, read when it is decoded.
    File(std::path::PathBuf),
    Pixels {
        bytes: Vec<u8>,
        width: u32,
        height: u32,
        alpha: bool,
    },
}

impl Input {
    fn of(source: Source<'_>, local_files: bool) -> Option<Self> {
        Some(match source {
            Source::Data(data) => Self::Encoded(
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .ok()?,
            ),
            Source::File(path) if local_files => {
                if std::fs::metadata(path).ok()?.len() > MOST_BYTES {
                    return None;
                }
                Self::File(path.into())
            }
            Source::File(_) => return None,
            Source::Bytes(bytes) => Self::Encoded(bytes.to_vec()),
            Source::Pixels {
                bytes,
                width,
                height,
                alpha,
            } => Self::Pixels {
                bytes: bytes.to_vec(),
                width,
                height,
                alpha,
            },
        })
    }

    /// How many pixels the image is, from its header: as much as is needed to know how
    /// much room it takes, without decoding it.
    fn dimensions(&self) -> Option<(u32, u32)> {
        match self {
            Self::Encoded(bytes) => image::ImageReader::new(std::io::Cursor::new(bytes))
                .with_guessed_format()
                .ok()?
                .into_dimensions()
                .ok(),
            Self::File(path) => image::ImageReader::open(path)
                .ok()?
                .with_guessed_format()
                .ok()?
                .into_dimensions()
                .ok(),
            Self::Pixels { width, height, .. } => Some((*width, *height)),
        }
    }

    fn decode(self) -> Option<image::DynamicImage> {
        match self {
            Self::Encoded(bytes) => image::load_from_memory(&bytes).ok(),
            Self::File(path) => image::load_from_memory(&std::fs::read(path).ok()?).ok(),
            Self::Pixels {
                bytes,
                width,
                height,
                alpha: true,
            } => image::RgbaImage::from_raw(width, height, bytes)
                .map(image::DynamicImage::ImageRgba8),
            Self::Pixels {
                bytes,
                width,
                height,
                alpha: false,
            } => {
                image::RgbImage::from_raw(width, height, bytes).map(image::DynamicImage::ImageRgb8)
            }
        }
    }
}

/// Encode a decoded image at the size the room allows.
fn make(picker: &Picker, image: image::DynamicImage, offered: Size) -> Option<SlicedProtocol> {
    // Fit shrinks but never enlarges, so an image smaller than the room it is given
    // is drawn at its own size rather than blown up.
    let natural = Resize::natural_size(&image, picker.font_size());
    let within = Size::new(
        natural.width.min(offered.width),
        natural.height.min(offered.height),
    );
    SlicedProtocol::new_with_resize(picker, image, within, Resize::Fit(None)).ok()
}

/// The cells an image of `width` by `height` pixels comes to once `make` has fitted it
/// into `offered`, worked out the way the encoder works it out, so that the rows kept
/// for an image still being made are the rows it fills.
fn fitted(width: u32, height: u32, font: FontSize, offered: Size) -> Size {
    let (cell_width, cell_height) = (u32::from(font.width), u32::from(font.height));
    let cells = |pixels: u32, cell: u32| pixels.div_ceil(cell).min(u32::from(u16::MAX)) as u16;
    // Never larger than the image itself, and never larger than the room.
    let room_width = (cells(width, cell_width).min(offered.width) as u32 * cell_width).min(width);
    let room_height =
        (cells(height, cell_height).min(offered.height) as u32 * cell_height).min(height);
    let ratio = f64::min(
        f64::from(room_width) / f64::from(width),
        f64::from(room_height) / f64::from(height),
    );
    let scaled = |pixels: u32| ((f64::from(pixels) * ratio).round() as u32).max(1);
    Size::new(
        cells(scaled(width), cell_width),
        cells(scaled(height), cell_height),
    )
}

/// Draw a placed image at `position` within `area`, clipped to it. The position may sit
/// above or below the area: a chat line scrolls, and half an image is still the image.
/// Where the key is ready at more than one size, the one drawn is the one last placed,
/// since placing moves it to the front.
pub fn draw(frame: &mut Frame, key: &str, area: Rect, position: SignedPosition) {
    STORE.with(|store| {
        let store = store.borrow();
        // While it is being made again at a new size, what it was made at before would
        // spill over or fall short of the rows kept for it.
        if store.pending.iter().any(|pending| pending.key == key) {
            return;
        }
        if let Some(ready) = store.ready.iter().find(|ready| ready.key == key) {
            frame.render_widget(SlicedImage::new(&ready.protocol, position), area);
        }
    });
}

/// Draw in half-blocks, which is the one way that needs no terminal to answer, and read
/// the files a test names.
#[cfg(test)]
pub fn draw_in_halfblocks() {
    STORE.with(|store| {
        let mut store = store.borrow_mut();
        store.picker = Some(Picker::halfblocks());
        store.local_files = true;
        store.reset();
    });
}

/// A PNG of a size a test can reason about, as base64.
#[cfg(test)]
pub fn test_png(width: u32, height: u32) -> String {
    let image =
        image::RgbImage::from_fn(width, height, |x, _| image::Rgb([(x % 256) as u8, 64, 128]));
    let mut bytes: Vec<u8> = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn an_image_takes_the_room_it_is_offered_and_no_more() {
        draw_in_halfblocks();
        let data = test_png(400, 400);
        let size = place("wide", Source::Data(&data), Size::new(40, 10))
            .expect("halfblocks draws anything");
        assert!(size.width <= 40 && size.height <= 10);
        // Square in, square out: a half-block cell is twice as tall as it is wide, so ten
        // rows of it are twenty cells' worth of picture.
        assert_eq!(size.height, 10);
        // Asked again, it answers from the encoding it already has.
        assert_eq!(
            place("wide", Source::Data(&data), Size::new(40, 10)),
            Some(size)
        );
    }

    /// The sidebar and the project picker draw the same icon at two sizes in one frame.
    /// Each has to find its own still there, or the terminal is sent both again on every
    /// frame and they flicker.
    #[test]
    fn one_image_at_two_sizes_keeps_both() {
        draw_in_halfblocks();
        let data = test_png(64, 64);
        let big = place("icon", Source::Data(&data), Size::new(4, 2)).unwrap();
        let small = place("icon", Source::Data(&data), Size::new(2, 1)).unwrap();
        assert_ne!(big, small);
        // Not an image, so this only answers if the first size was kept.
        assert_eq!(
            place("icon", Source::Data("bm90IGFuIGltYWdl"), Size::new(4, 2)),
            Some(big)
        );
        assert_eq!(
            place("icon", Source::Data("bm90IGFuIGltYWdl"), Size::new(2, 1)),
            Some(small)
        );
    }

    /// Wait for the threads to finish what they were given, as the screen would.
    fn settle() {
        for _ in 0..1000 {
            let busy = STORE.with(|store| {
                let mut store = store.borrow_mut();
                store.collect();
                !store.pending.is_empty()
            });
            if !busy {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the image was never made");
    }

    /// A large image keeps its rows at once and is drawn into them once it is made, and
    /// the rows kept are the rows it fills, whatever its shape and whatever the room.
    #[test]
    fn a_large_image_is_made_aside_into_the_rows_kept_for_it() {
        draw_in_halfblocks();
        for (width, height, room) in [
            (1200, 1200, Size::new(40, 10)),
            (1600, 700, Size::new(80, 30)),
            (700, 1600, Size::new(80, 30)),
            (2922, 1816, Size::new(150, 40)),
            (900, 600, Size::new(500, 500)),
        ] {
            let key = format!("large-{width}x{height}");
            let data = test_png(width, height);
            let kept = place(&key, Source::Data(&data), room).expect("an image");
            STORE.with(|store| assert_eq!(store.borrow().pending.len(), 1));
            settle();
            assert_eq!(
                place(&key, Source::Data(&data), room),
                Some(kept),
                "{width}x{height} in {room:?}"
            );
        }
    }

    #[test]
    fn a_large_image_that_will_not_decode_takes_no_room_once_tried() {
        draw_in_halfblocks();
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(test_png(1200, 1200))
            .unwrap();
        // The header survives, and says how big it is; the pixels do not.
        bytes.truncate(100);
        assert!(place("cut", Source::Bytes(&bytes), Size::new(40, 10)).is_some());
        settle();
        assert_eq!(place("cut", Source::Bytes(&bytes), Size::new(40, 10)), None);
    }

    #[test]
    fn what_is_not_an_image_takes_no_room() {
        draw_in_halfblocks();
        assert_eq!(
            place("junk", Source::Data("bm90IGFuIGltYWdl"), Size::new(40, 10)),
            None
        );
        assert_eq!(
            place("empty", Source::Data(&test_png(4, 4)), Size::new(0, 10)),
            None
        );
    }

    /// A thread's own rows name a file rather than carrying the picture, so the file is
    /// the image — but only where the path means this machine.
    #[test]
    fn a_named_file_is_read_when_the_disk_is_ours() {
        draw_in_halfblocks();
        let path = std::env::temp_dir().join("tria-a-named-file.png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(test_png(80, 80))
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let path = path.to_str().unwrap();
        assert!(place("named", Source::File(path), Size::new(20, 6)).is_some());
        assert_eq!(
            place("missing", Source::File("/nowhere.png"), Size::new(20, 6)),
            None
        );

        // Against a server on another machine the path is not ours to follow: the same
        // name here is either nothing or somebody else's file.
        STORE.with(|store| {
            let mut store = store.borrow_mut();
            store.local_files = false;
            store.ready.clear();
        });
        assert_eq!(place("named", Source::File(path), Size::new(20, 6)), None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn only_the_part_of_an_image_inside_the_area_is_drawn() {
        draw_in_halfblocks();
        let size = place(
            "scrolled",
            Source::Data(&test_png(80, 80)),
            Size::new(20, 6),
        )
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
        // Two lines above the top of the area: the first two rows of the image are gone
        // and the rest has moved up.
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 30, 8);
                draw(frame, "scrolled", area, SignedPosition::from((2, -2)));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        // Half-blocks are colour rather than glyphs: a cell of the image is one the
        // protocol has painted.
        let drawn = |x: u16, y: u16| buffer[(x, y)].bg != ratatui::style::Color::Reset;
        assert!(drawn(2, 0), "the image starts at the left of its indent");
        assert!(!drawn(1, 0), "and not before it");
        let left = size.height - 2;
        assert!(drawn(2, left - 1), "down to the last row still in the area");
        assert!(!drawn(2, left), "and no further");
    }

    #[test]
    fn what_the_terminal_held_is_dropped_when_the_screen_goes() {
        draw_in_halfblocks();
        place("gone", Source::Data(&test_png(8, 8)), Size::new(20, 6)).unwrap();
        forget();
        STORE.with(|store| assert!(store.borrow().ready.is_empty()));
    }
}
