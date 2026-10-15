//! Integration tests for the Pdfium worker. They need the native Pdfium library
//! (see `scripts/download_pdfium.sh`) and are skipped when it is unavailable.

use std::sync::mpsc;
use std::time::Duration;

use kkpdf_zed::{DocumentInfo, PdfWorker, RenderedTile, TileRequest};

/// Builds a one-page 200x100 pt PDF with a black bar along the top, a red square,
/// and a small embedded light-gray raster image.
fn fixture_pdf() -> Vec<u8> {
    let content = b"0 0 0 rg 0 90 200 10 re f\n\
                    1 0 0 rg 20 20 40 40 re f\n\
                    q 40 0 0 40 120 20 cm /Im1 Do Q\n";
    let image: Vec<u8> = vec![0xC8; 2 * 2 * 3];
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R \
           /Resources << /XObject << /Im1 5 0 R >> >> >>"
            .to_vec(),
    ];
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content);
    stream.extend_from_slice(b"\nendstream");
    objects.push(stream);
    let mut image_stream = format!(
        "<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB \
         /BitsPerComponent 8 /Length {} >>\nstream\n",
        image.len()
    )
    .into_bytes();
    image_stream.extend_from_slice(&image);
    image_stream.extend_from_slice(b"\nendstream");
    objects.push(image_stream);

    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

fn worker() -> Option<&'static PdfWorker> {
    match PdfWorker::global() {
        Ok(worker) => Some(worker),
        Err(error) => {
            eprintln!("skipping: {error}");
            None
        }
    }
}

fn open(worker: &PdfWorker, bytes: Vec<u8>) -> anyhow::Result<DocumentInfo> {
    let (tx, rx) = mpsc::channel();
    worker.open(bytes, move |result| {
        tx.send(result).ok();
    });
    rx.recv_timeout(Duration::from_secs(10))?
}

fn render(worker: &PdfWorker, request: TileRequest) -> RenderedTile {
    let (tx, rx) = mpsc::channel();
    let _ticket = worker.render(request, move |result| {
        tx.send(result).ok();
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("render timed out")
        .expect("render failed")
}

fn rgb(tile: &RenderedTile, x: u32, y: u32) -> [u8; 3] {
    let pixel = &tile.bgra[((y * tile.width + x) * 4) as usize..][..4];
    [pixel[2], pixel[1], pixel[0]]
}

fn full_page(document: u64, scale: f32) -> TileRequest {
    TileRequest {
        document,
        page_index: 0,
        scale,
        x: 0,
        y: 0,
        width: (200.0 * scale) as u32,
        height: (100.0 * scale) as u32,
    }
}

#[test]
fn opens_documents_and_reports_page_geometry() {
    let Some(worker) = worker() else { return };
    let info = open(worker, fixture_pdf()).expect("fixture should open");
    assert_eq!(info.pages.len(), 1);
    assert_eq!((info.pages[0].width, info.pages[0].height), (200.0, 100.0));
    worker.close(info.id);
}

#[test]
fn rejects_non_pdf_bytes() {
    let Some(worker) = worker() else { return };
    assert!(open(worker, b"definitely not a pdf".to_vec()).is_err());
}

#[test]
fn renders_page_content_at_the_right_place() {
    let Some(worker) = worker() else { return };
    let info = open(worker, fixture_pdf()).unwrap();
    let tile = render(worker, full_page(info.id, 2.0));
    assert_eq!((tile.width, tile.height), (400, 200));
    // Black bar spans the top 10pt (20px); PDF y grows upward, device y downward.
    assert_eq!(rgb(&tile, 200, 5), [0, 0, 0]);
    // Red square covers x 20..60pt and y 20..60pt from the bottom.
    assert_eq!(rgb(&tile, 80, 120), [255, 0, 0]);
    // Paper.
    assert_eq!(rgb(&tile, 200, 100), [255, 255, 255]);
    worker.close(info.id);
}

#[test]
fn tiles_match_the_full_page_render() {
    let Some(worker) = worker() else { return };
    let info = open(worker, fixture_pdf()).unwrap();
    let scale = 3.0;
    let full = render(worker, full_page(info.id, scale));
    let (tile_x, tile_y, tile_w, tile_h) = (37, 51, 128, 96);
    let tile = render(
        worker,
        TileRequest {
            x: tile_x,
            y: tile_y,
            width: tile_w,
            height: tile_h,
            ..full_page(info.id, scale)
        },
    );
    for y in 0..tile_h {
        for x in 0..tile_w {
            assert_eq!(
                rgb(&tile, x, y),
                rgb(&full, tile_x + x, tile_y + y),
                "mismatch at tile pixel ({x}, {y})"
            );
        }
    }
    worker.close(info.id);
}

#[test]
fn closed_documents_cannot_be_rendered() {
    let Some(worker) = worker() else { return };
    let info = open(worker, fixture_pdf()).unwrap();
    worker.close(info.id);
    let (tx, rx) = mpsc::channel();
    let _ticket = worker.render(full_page(info.id, 1.0), move |result| {
        tx.send(result.is_err()).ok();
    });
    assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap());
}
