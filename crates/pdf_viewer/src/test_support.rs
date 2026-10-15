//! Generated PDF documents for tests and visual tests.

/// Assembles a PDF from object bodies, numbering them from 1 and writing a valid xref table.
fn assemble(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
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

fn stream(dictionary: &str, data: &[u8]) -> Vec<u8> {
    let mut object = format!("<< {dictionary} /Length {} >>\nstream\n", data.len()).into_bytes();
    object.extend_from_slice(data);
    object.extend_from_slice(b"\nendstream");
    object
}

/// `pages` US Letter pages, each with a black bar near the top.
pub fn multi_page_pdf(pages: usize) -> Vec<u8> {
    let kids = (0..pages)
        .map(|page| format!("{} 0 R", 4 + page))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>").into_bytes(),
        stream("", b"0 0 0 rg 72 700 468 20 re f\n"),
    ];
    for _ in 0..pages {
        objects.push(
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 3 0 R >>".to_vec(),
        );
    }
    assemble(&objects)
}

/// A realistic three-page document: headings, body text, colored vector shapes
/// and an embedded photo-like raster image, for checking rendering.
pub fn sample_document() -> Vec<u8> {
    let page_contents = (1..=3)
        .map(|page| {
            format!(
                "BT /F2 26 Tf 0.1 0.2 0.55 rg 72 700 Td (Chapter {page}: Vectors) Tj ET\n\
                 BT /F1 12 Tf 0 0 0 rg 72 668 Td 16 TL\n\
                 (A vector has both a magnitude and a direction. Two vectors are equal) Tj T*\n\
                 (when they have the same magnitude and point the same way.) Tj T*\n\
                 T* (1. Find the length of the vector from A\\(1, 2\\) to B\\(4, 6\\).) Tj T*\n\
                 (2. Show that the diagonals of a parallelogram bisect each other.) Tj ET\n\
                 0.85 0.15 0.15 rg 72 470 120 60 re f\n\
                 0.95 0.85 0.3 rg 210 470 120 60 re f\n\
                 0 0 0 RG 1.5 w 348 470 120 60 re S\n\
                 q 180 0 0 120 72 300 cm /Im1 Do Q\n\
                 BT /F1 10 Tf 0.4 0.4 0.4 rg 290 60 Td (Page {page} of 3) Tj ET\n"
            )
            .into_bytes()
        })
        .collect::<Vec<_>>();

    // A smooth sky-to-grass gradient, standing in for a photograph.
    let (width, height) = (96u32, 64u32);
    let mut image = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let t = y as f32 / height as f32;
            let sky = [110., 170., 235.];
            let grass = [70., 150., 60.];
            let shade = 0.85 + 0.15 * (x as f32 / width as f32);
            for channel in 0..3 {
                let value = sky[channel] + (grass[channel] - sky[channel]) * t;
                image.push((value * shade) as u8);
            }
        }
    }

    // 1 catalog, 2 pages, 3 font, 4 bold font, 5 image, 6..8 contents, 9..11 pages.
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [9 0 R 10 0 R 11 0 R] /Count 3 >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".to_vec(),
        stream(
            &format!(
                "/Type /XObject /Subtype /Image /Width {width} /Height {height} \
                 /ColorSpace /DeviceRGB /BitsPerComponent 8"
            ),
            &image,
        ),
    ];
    for contents in &page_contents {
        objects.push(stream("", contents));
    }
    for page in 0..3 {
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R >> /XObject << /Im1 5 0 R >> >> >>",
                6 + page
            )
            .into_bytes(),
        );
    }
    assemble(&objects)
}
