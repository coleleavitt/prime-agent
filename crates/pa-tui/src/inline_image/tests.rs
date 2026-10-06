use super::painter::Painter;
use super::payload::{
    encode_png_rgb, jpeg_base64_to_png, shrink_rgb, KittyPayload, KittyPayloadState, PayloadSource,
};
use super::plan::{plan, Visible};
use super::*;
use crate::terminal_image::{
    clear_image_protocol_override, delete_kitty_image, encode_iterm2, encode_kitty,
    kitty_delete_placements, kitty_place, kitty_transmit, set_cell_dimensions_override,
    set_image_protocol_override, Iterm2Options, Iterm2Size, KittyCrop, KittyOptions,
};
use base64::Engine;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read as _;

const CELL: CellDimensions = CellDimensions {
    width_px: 10,
    height_px: 20,
};

fn dims(width_px: u32, height_px: u32) -> ImageDimensions {
    ImageDimensions {
        width_px,
        height_px,
    }
}

/// The thread's image state for one test, restored on drop.
struct Terminal;

impl Terminal {
    fn with(protocol: Option<ImageProtocol>) -> Self {
        set_image_protocol_override(protocol);
        set_cell_dimensions_override(Some(CELL));
        Self
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        clear_image_protocol_override();
        set_cell_dimensions_override(None);
    }
}

#[test]
fn the_block_follows_ts_image_geometry_and_caps_tall_previews() {
    // 60 columns (TS `maxWidthCells`), `calculateImageRows` rows.
    assert_eq!(
        block_geometry(dims(1600, 900), 120, CELL),
        Some(ImageBlock {
            columns: 60,
            rows: 17
        })
    );
    // A narrow pane takes the width under the branch indent.
    assert_eq!(
        block_geometry(dims(1600, 900), 44, CELL),
        Some(ImageBlock {
            columns: 40,
            rows: 12
        })
    );
    // A portrait preview narrows until it fits 24 rows, aspect intact.
    assert_eq!(
        block_geometry(dims(800, 1600), 120, CELL),
        Some(ImageBlock {
            columns: 24,
            rows: 24
        })
    );
    // Too narrow, or no size: the textual fallback.
    assert_eq!(block_geometry(dims(1600, 900), 11, CELL), None);
    assert_eq!(block_geometry(dims(0, 900), 120, CELL), None);
}

#[test]
fn the_block_needs_a_protocol_that_takes_the_type() {
    let png = PanelImage::new("aGk=", "image/png", dims(1600, 900));
    let gif = PanelImage::new("aGk=", "image/gif", dims(1600, 900));
    {
        let _terminal = Terminal::with(None);
        assert_eq!(image_block(&png, 80), None);
    }
    {
        let _terminal = Terminal::with(Some(ImageProtocol::Kitty));
        assert!(image_block(&png, 80).is_some());
        // kitty takes PNG (and JPEG through the transcode), never GIF.
        assert_eq!(image_block(&gif, 80), None);
        // The exit flush's scrollback keeps the fallback.
        assert_eq!(with_text_fallback(|| image_block(&png, 80)), None);
    }
    {
        let _terminal = Terminal::with(Some(ImageProtocol::Iterm2));
        assert!(image_block(&gif, 80).is_some());
    }
}

#[test]
fn markers_round_trip_and_strip() {
    let image = PanelImage::new("aGk=", "image/png", dims(100, 40));
    let rows = image_block_rows(
        &image,
        ImageBlock {
            columns: 10,
            rows: 2,
        },
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(
        parse_marker(&rows[1][0].content),
        Some(Marker {
            key: image.key,
            index: 1,
            rows: 2,
            column: 4,
            columns: 10,
        })
    );
    // Zero width: the row measures as its indent alone.
    assert_eq!(crate::width::line_width(&rows[0]), 4);
    let mut stripped = rows[0].clone();
    strip_markers(&mut stripped);
    assert_eq!(stripped, vec![Span::raw("    ")]);
    assert_eq!(parse_marker("\x1b_pa-image;zz;0;1;4;10\x1b\\"), None);
    assert_eq!(parse_marker("\x1b_pa-image;1f;2;2;4;10\x1b\\"), None);
}

/// A frame of `height` blank rows with `image`'s block from `top` (rows
/// before 0 scrolled out).
fn frame_with(image: &PanelImage, block: ImageBlock, top: isize, height: usize) -> Vec<Line> {
    let rows = image_block_rows(image, block);
    (0..height)
        .map(|row| {
            let index = row as isize - top;
            usize::try_from(index)
                .ok()
                .and_then(|index| rows.get(index).cloned())
                .unwrap_or_else(|| vec![Span::raw(" ".repeat(40))])
        })
        .collect()
}

const BLOCK: ImageBlock = ImageBlock {
    columns: 20,
    rows: 5,
};

#[test]
fn the_plan_finds_whole_scrolled_and_covered_bands() {
    let image = PanelImage::new("cGxhbg==", "image/png", dims(200, 100));
    let band = |row, first, rows| Visible {
        key: image.key,
        row,
        column: 4,
        columns: 20,
        first,
        rows,
        total: 5,
    };
    assert_eq!(plan(&frame_with(&image, BLOCK, 3, 12)), vec![band(3, 0, 5)]);
    // Two rows scrolled out at the top; one row clipped at the bottom.
    assert_eq!(
        plan(&frame_with(&image, BLOCK, -2, 12)),
        vec![band(0, 2, 3)]
    );
    assert_eq!(plan(&frame_with(&image, BLOCK, 8, 12)), vec![band(8, 0, 4)]);
    assert_eq!(plan(&frame_with(&image, BLOCK, 12, 12)), Vec::new());
    // An overlay painted over the image cells of the middle row splits the
    // band; the taller half shows. Text beside the image cells does not.
    let mut covered = frame_with(&image, BLOCK, 3, 12);
    let marker = covered[4][0].clone();
    covered[4] = vec![
        marker.clone(),
        Span::raw(" ".repeat(24)),
        Span::raw("beside"),
    ];
    assert_eq!(plan(&covered), vec![band(3, 0, 5)]);
    covered[4] = vec![marker, Span::raw("      overlay text   ")];
    assert_eq!(plan(&covered), vec![band(5, 2, 3)]);
}

/// A fixed payload source.
#[derive(Default)]
struct Stub {
    kitty: HashMap<u64, KittyPayloadState>,
    files: HashMap<u64, Arc<str>>,
}

impl PayloadSource for Stub {
    fn file(&self, key: u64) -> Option<Arc<str>> {
        self.files.get(&key).cloned()
    }

    fn kitty(&self, key: u64) -> KittyPayloadState {
        self.kitty
            .get(&key)
            .cloned()
            .unwrap_or(KittyPayloadState::Unavailable)
    }
}

fn kitty_stub(image: &PanelImage) -> Stub {
    let mut stub = Stub::default();
    stub.kitty.insert(
        image.key,
        KittyPayloadState::Ready(Arc::new(KittyPayload {
            base64: Arc::from("UE5HREFUQQ=="),
            width_px: 200,
            height_px: 100,
        })),
    );
    stub
}

fn kitty_id(escapes: &str) -> u32 {
    regex::Regex::new(r"i=(\d+)")
        .expect("regex")
        .captures(escapes)
        .and_then(|captures| captures[1].parse().ok())
        .expect("an image id")
}

#[test]
fn kitty_places_once_moves_by_id_and_deletes_when_scrolled_out() {
    let image = PanelImage::new("a2l0dHk=", "image/png", dims(200, 100));
    let stub = kitty_stub(&image);
    let mut painter = Painter::default();
    let paint = |painter: &mut Painter, frame: &[Line]| {
        let mut out = String::new();
        painter.paint(ImageProtocol::Kitty, frame, (40, 12), &stub, &mut out);
        out
    };
    // First sight, whole: TS's transmit-and-place at the reserved origin.
    let first = paint(&mut painter, &frame_with(&image, BLOCK, 3, 12));
    let id = kitty_id(&first);
    assert_eq!(
        first,
        format!(
            "\x1b[4;5H{}",
            encode_kitty(
                "UE5HREFUQQ==",
                &KittyOptions {
                    columns: Some(20),
                    rows: Some(5),
                    image_id: Some(id),
                    move_cursor: false,
                }
            )
        )
    );
    // An unchanged frame writes nothing.
    assert_eq!(paint(&mut painter, &frame_with(&image, BLOCK, 3, 12)), "");
    // A scroll re-places the stored image: no re-send.
    assert_eq!(
        paint(&mut painter, &frame_with(&image, BLOCK, 2, 12)),
        format!(
            "{}\x1b[3;5H{}",
            kitty_delete_placements(id),
            kitty_place(id, 20, 5, None)
        )
    );
    // Partly scrolled out: the visible band of the source, cropped.
    assert_eq!(
        paint(&mut painter, &frame_with(&image, BLOCK, -2, 12)),
        format!(
            "{}\x1b[1;5H{}",
            kitty_delete_placements(id),
            kitty_place(
                id,
                20,
                3,
                Some(KittyCrop {
                    y: 40,
                    width: 200,
                    height: 60
                })
            )
        )
    );
    // Gone from the frame (scrolled out, or the view switched).
    assert_eq!(
        paint(&mut painter, &frame_with(&image, BLOCK, 20, 12)),
        kitty_delete_placements(id)
    );
    // The surface's exit frees the data.
    let mut released = String::new();
    painter.release(&mut released);
    assert_eq!(released, delete_kitty_image(id));
    let mut again = String::new();
    painter.release(&mut again);
    assert_eq!(again, "");
}

#[test]
fn kitty_first_seen_cropped_transmits_then_places() {
    let image = PanelImage::new("Y3JvcA==", "image/png", dims(200, 100));
    let stub = kitty_stub(&image);
    let mut painter = Painter::default();
    let mut out = String::new();
    painter.paint(
        ImageProtocol::Kitty,
        &frame_with(&image, BLOCK, -4, 12),
        (40, 12),
        &stub,
        &mut out,
    );
    let id = kitty_id(&out);
    assert_eq!(
        out,
        format!(
            "\x1b[1;5H{}{}",
            kitty_transmit("UE5HREFUQQ==", id),
            kitty_place(
                id,
                20,
                1,
                Some(KittyCrop {
                    y: 80,
                    width: 200,
                    height: 20
                })
            )
        )
    );
}

#[test]
fn kitty_waits_for_a_pending_transcode_and_re_places_after_a_resize() {
    let image = PanelImage::new("cGVuZGluZw==", "image/jpeg", dims(200, 100));
    let mut stub = Stub::default();
    stub.kitty.insert(image.key, KittyPayloadState::Pending);
    let mut painter = Painter::default();
    let frame = frame_with(&image, BLOCK, 3, 12);
    let mut out = String::new();
    painter.paint(ImageProtocol::Kitty, &frame, (40, 12), &stub, &mut out);
    assert_eq!(out, "", "nothing to place while the transcode runs");
    let stub = kitty_stub(&image);
    painter.paint(ImageProtocol::Kitty, &frame, (40, 12), &stub, &mut out);
    let id = kitty_id(&out);
    // The resize's clear: the old placement goes, and the image is sent
    // again (kitty freed the data the clear erased).
    let mut resized = String::new();
    painter.paint(ImageProtocol::Kitty, &frame, (50, 12), &stub, &mut resized);
    assert_eq!(
        resized,
        format!(
            "{}\x1b[4;5H{}",
            kitty_delete_placements(id),
            encode_kitty(
                "UE5HREFUQQ==",
                &KittyOptions {
                    columns: Some(20),
                    rows: Some(5),
                    image_id: Some(id),
                    move_cursor: false,
                }
            )
        )
    );
}

#[test]
fn iterm2_places_whole_previews_and_repaints_the_cells_a_moved_one_covered() {
    let image = PanelImage::new("aXRlcm0=", "image/png", dims(200, 100));
    let mut stub = Stub::default();
    stub.files.insert(image.key, Arc::from("RklMRQ=="));
    let mut painter = Painter::default();
    let placement = |row: u16| {
        format!(
            "\x1b[{};5H{}",
            row + 1,
            encode_iterm2(
                "RklMRQ==",
                &Iterm2Options {
                    width: Some(Iterm2Size::Cells(20)),
                    height: Some(Iterm2Size::Auto),
                    ..Iterm2Options::default()
                }
            )
        )
    };
    let mut out = String::new();
    painter.paint(
        ImageProtocol::Iterm2,
        &frame_with(&image, BLOCK, 3, 12),
        (40, 12),
        &stub,
        &mut out,
    );
    assert_eq!(out, placement(3));
    // Moved up one row: rows 3..8 repaint from the frame, then the image
    // goes out at its new origin.
    let moved = frame_with(&image, BLOCK, 2, 12);
    let mut out = String::new();
    painter.paint(ImageProtocol::Iterm2, &moved, (40, 12), &stub, &mut out);
    let mut expected = String::new();
    for row in 3..8u16 {
        let mut line = moved[usize::from(row)].clone();
        strip_markers(&mut line);
        let _ = write!(
            expected,
            "\x1b[{};1H\x1b[2K{}",
            row + 1,
            crate::ansi::line_to_ansi(&line)
        );
    }
    expected.push_str(&placement(2));
    assert_eq!(out, expected);
    // iTerm2 cannot crop: a partly scrolled preview stays off screen, and
    // a block touching the last row would scroll it.
    for top in [-1, 7] {
        let mut out = String::new();
        let mut fresh = Painter::default();
        fresh.paint(
            ImageProtocol::Iterm2,
            &frame_with(&image, BLOCK, top, 12),
            (40, 12),
            &stub,
            &mut out,
        );
        assert_eq!(out, "", "top {top}");
    }
}

#[test]
fn the_png_encoder_writes_a_valid_stream() {
    let rgb = [255, 0, 0, 0, 0, 255, 1, 2, 3, 4, 5, 6];
    let png = encode_png_rgb(&rgb, 2, 2);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    // IHDR: 2x2, 8-bit RGB.
    assert_eq!(&png[8..16], b"\x00\x00\x00\x0dIHDR");
    assert_eq!(&png[16..29], &[0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0]);
    // The IEND chunk and its well-known CRC.
    assert_eq!(
        &png[png.len() - 12..],
        b"\x00\x00\x00\x00IEND\xae\x42\x60\x82"
    );
    // IDAT inflates to filter-0 scanlines.
    let idat_len = u32::from_be_bytes([png[33], png[34], png[35], png[36]]) as usize;
    assert_eq!(&png[37..41], b"IDAT");
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&png[41..41 + idat_len])
        .read_to_end(&mut raw)
        .expect("zlib");
    assert_eq!(raw, [0, 255, 0, 0, 0, 0, 255, 0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(
        crate::terminal_image::get_image_dimensions_prefix(
            &base64::engine::general_purpose::STANDARD.encode(&png),
            "image/png",
            crate::terminal_image::IMAGE_DIMENSIONS_PREFIX_BYTES,
        ),
        Some(dims(2, 2))
    );
}

#[test]
fn shrinking_averages_whole_and_partial_boxes() {
    // 3x1 grey ramp halves to 2x1: (0+100)/2 and the edge pixel alone.
    let rgb = [0, 0, 0, 100, 100, 100, 50, 60, 70];
    assert_eq!(
        shrink_rgb(&rgb, 3, 1, 2),
        (vec![50, 50, 50, 50, 60, 70], 2, 1)
    );
    assert_eq!(shrink_rgb(&rgb, 3, 1, 1), (rgb.to_vec(), 3, 1));
}

/// Decode kitty's PNG back to scanlines (test-side inflate).
fn png_scanlines(payload: &KittyPayload) -> Vec<u8> {
    let png = base64::engine::general_purpose::STANDARD
        .decode(payload.base64.as_bytes())
        .expect("base64");
    let idat_len = u32::from_be_bytes([png[33], png[34], png[35], png[36]]) as usize;
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&png[41..41 + idat_len])
        .read_to_end(&mut raw)
        .expect("zlib");
    raw
}

#[test]
fn a_jpeg_preview_becomes_a_png_kitty_accepts() {
    let jpeg = base64::engine::general_purpose::STANDARD
        .encode(include_bytes!("fixtures/split-32x16.jpg"));
    let payload = jpeg_base64_to_png(&jpeg).expect("the JPEG transcodes");
    assert_eq!((payload.width_px, payload.height_px), (32, 16));
    let raw = png_scanlines(&payload);
    assert_eq!(raw.len(), (32 * 3 + 1) * 16);
    // Left half red, right half blue (JPEG-lossy).
    let pixel = |x: usize, y: usize| {
        let at = y * (32 * 3 + 1) + 1 + x * 3;
        [raw[at], raw[at + 1], raw[at + 2]]
    };
    let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 24);
    assert!(near(pixel(2, 8), [255, 0, 0]), "{:?}", pixel(2, 8));
    assert!(near(pixel(29, 8), [0, 0, 255]), "{:?}", pixel(29, 8));
    // An 1100-pixel edge halves under the 1024-pixel cap.
    let wide = base64::engine::general_purpose::STANDARD
        .encode(include_bytes!("fixtures/green-1100x500.jpg"));
    let payload = jpeg_base64_to_png(&wide).expect("the JPEG transcodes");
    assert_eq!((payload.width_px, payload.height_px), (550, 250));
    assert!(matches!(
        jpeg_base64_to_png("bm90IGEganBlZw=="),
        Err(super::payload::TranscodeError::Decode(_))
    ));
    assert_eq!(
        jpeg_base64_to_png("!!"),
        Err(super::payload::TranscodeError::Base64)
    );
}

/// The process source hands PNG through untouched and transcodes a JPEG
/// off the paint path, waking the session loop when it settles.
#[test]
fn the_global_source_prepares_jpeg_off_the_paint_path() {
    let png_bytes = encode_png_rgb(&[9, 9, 9], 1, 1);
    let png = PanelImage::new(
        &base64::engine::general_purpose::STANDARD.encode(&png_bytes),
        "image/png",
        dims(1, 1),
    );
    let KittyPayloadState::Ready(payload) = GlobalSource.kitty(png.key) else {
        panic!("a PNG is ready at once");
    };
    assert_eq!(&*payload.base64, &*png.data.0);
    let jpeg = PanelImage::new(
        &base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("fixtures/split-32x16.jpg")),
        "image/jpeg",
        dims(32, 16),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    assert_eq!(GlobalSource.kitty(jpeg.key), KittyPayloadState::Pending);
    runtime.block_on(async {
        loop {
            if let KittyPayloadState::Ready(payload) = GlobalSource.kitty(jpeg.key) {
                assert_eq!((payload.width_px, payload.height_px), (32, 16));
                break;
            }
            tokio::time::timeout(std::time::Duration::from_secs(30), payload_ready())
                .await
                .expect("the transcode settles");
        }
    });
}

fn presented_artifact() -> Vec<crate::chat::ChatEntry> {
    crate::custom_message::custom_message_entries(&serde_json::json!({
        "role": "custom",
        "customType": crate::custom_message::PRESENTED_ARTIFACT_CUSTOM_TYPE,
        "content": [
            { "type": "text", "text": "Direction A" },
            { "type": "image", "data": "ZnJhbWU=", "mimeType": "image/png" }
        ],
        "display": true,
        "details": { "name": "render.png", "kind": "image", "mimeType": "image/png",
                     "width": 1600, "height": 900 },
    }))
}

fn session_view() -> crate::view::AgentView {
    let mut view = crate::view::AgentView::new(crate::theme::Theme::builtin(
        "prime",
        crate::theme::ColorMode::TrueColor,
    ));
    for entry in presented_artifact() {
        view.push_entry(entry);
    }
    view
}

/// The whole session frame: the preview's 17 reserved rows sit under its
/// label, blank in the painted cells, and the exit flush writes the
/// textual fallback into scrollback, never an image escape.
#[test]
fn the_session_frame_reserves_the_preview_and_the_exit_flush_keeps_its_text() {
    let _terminal = Terminal::with(Some(ImageProtocol::Kitty));
    let mut view = session_view();
    let frame = view.render_frame(80, 40);
    let bands = plan(&frame);
    let [band] = bands.as_slice() else {
        panic!("one preview band: {bands:?}");
    };
    assert_eq!(
        (band.column, band.columns, band.first, band.rows, band.total),
        (4, 60, 0, 17, 17)
    );
    let text = crate::app::render_frame_text(&mut view, 80, 40);
    let top = usize::from(band.row);
    assert_eq!(text[top - 1].trim_end(), " \u{2570}\u{2500} Direction A");
    assert!(
        text[top..top + 17].iter().all(|row| row.trim().is_empty()),
        "{text:#?}"
    );
    assert!(!text.iter().any(|row| row.contains("[Image:")));
    // The cell paint never sees a marker.
    for line in &frame {
        let painted = crate::markdown::to_ratatui_line(line);
        assert!(!painted
            .spans
            .iter()
            .any(|span| span.content.contains("pa-image")));
    }
    let mut scrollback: Vec<u8> = Vec::new();
    view.stream_flush_to(&mut scrollback, 80, 40)
        .expect("the flush streams");
    let scrollback = String::from_utf8_lossy(&scrollback);
    assert!(scrollback.contains("[Image: render.png [image/png] 1600x900]"));
    for escape in ["\x1b_G", "\x1b]1337;", "pa-image"] {
        assert!(!scrollback.contains(escape), "{escape:?} leaked");
    }
    // The next frame reserves the block again.
    assert_eq!(plan(&view.render_frame(80, 40)).len(), 1);
}

/// tmux, screen, and unknown terminals keep the `[Image: …]` panel.
#[test]
fn the_session_frame_keeps_the_text_fallback_without_a_protocol() {
    let _terminal = Terminal::with(None);
    let mut view = session_view();
    assert_eq!(plan(&view.render_frame(80, 40)), Vec::new());
    let text = crate::app::render_frame_text(&mut view, 80, 40);
    assert!(
        text.iter()
            .any(|row| row.trim_end() == "    [Image: render.png [image/png] 1600x900]"),
        "{text:#?}"
    );
}

/// New rows push the preview up: the band shrinks from the top as its
/// rows leave the transcript window, then the image leaves the plan.
#[test]
fn a_preview_scrolling_out_of_the_window_shrinks_to_its_visible_band() {
    let _terminal = Terminal::with(Some(ImageProtocol::Kitty));
    let mut view = session_view();
    let mut seen_cropped = false;
    for turn in 0..60 {
        view.push(crate::session::TranscriptItem::UserMessage {
            text: format!("turn {turn}"),
        });
        let bands = plan(&view.render_frame(80, 30));
        match bands.as_slice() {
            [band] if band.first > 0 => {
                seen_cropped = true;
                assert_eq!(band.first + band.rows, band.total, "{band:?}");
            }
            [band] => assert!(!seen_cropped, "the band never regrows: {band:?}"),
            [] => {
                assert!(seen_cropped, "the band shrank before it left");
                return;
            }
            more => panic!("one preview at most: {more:?}"),
        }
    }
    panic!("the preview never scrolled out");
}
