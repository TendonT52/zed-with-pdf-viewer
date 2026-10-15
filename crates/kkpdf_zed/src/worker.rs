//! Dedicated Pdfium worker thread for interactive viewers.
//!
//! Pdfium is not thread-safe, and re-parsing a document for every render is
//! wasteful, so a single long-lived thread owns the Pdfium instance together with
//! every open document and a small cache of loaded pages. Callers submit jobs and
//! receive results through callbacks. Render jobs are served newest-first, because
//! the most recent request is the one that matches what is on screen right now,
//! and jobs whose [`RenderTicket`] was dropped are skipped without rendering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use anyhow::{Context as _, Result, anyhow};
use pdfium_render::prelude::*;

use crate::document::PageDimensions;
use crate::pdfium::{loader, validate_pdf_bytes};

/// Identifies a document opened in the worker.
pub type DocumentId = u64;

/// Largest tile edge, in device pixels, that the worker will rasterize.
pub const MAX_TILE_EDGE: u32 = 4096;

/// Number of loaded pages kept per document, so consecutive tiles of the same
/// page don't re-parse its content stream.
const LOADED_PAGE_CACHE: usize = 8;

/// Page geometry and metadata for a document opened in the worker.
#[derive(Debug, Clone)]
pub struct DocumentInfo {
    pub id: DocumentId,
    pub pages: Vec<PageDimensions>,
    pub title: Option<String>,
}

/// A request to rasterize one rectangular tile of a page.
///
/// The page is scaled by `scale` device pixels per PDF point, and the tile covers
/// the device-pixel rectangle starting at (`x`, `y`) with the given size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileRequest {
    pub document: DocumentId,
    pub page_index: usize,
    pub scale: f32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A rasterized tile in BGRA8 order (the order GPU texture atlases expect).
#[derive(Debug, Clone)]
pub struct RenderedTile {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Cancels the associated render job when dropped, unless it already ran.
#[derive(Debug)]
pub struct RenderTicket {
    cancelled: Arc<AtomicBool>,
}

impl Drop for RenderTicket {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

type Reply<T> = Box<dyn FnOnce(Result<T>) + Send>;

enum Job {
    Open {
        id: DocumentId,
        bytes: Vec<u8>,
        reply: Reply<DocumentInfo>,
    },
    Close(DocumentId),
    Render {
        request: TileRequest,
        cancelled: Arc<AtomicBool>,
        reply: Reply<RenderedTile>,
    },
}

#[derive(Default)]
struct Queue {
    /// Open and close jobs, served first and in order.
    control: VecDeque<Job>,
    /// Render jobs, served newest-first.
    renders: Vec<Job>,
}

/// Handle to the process-wide Pdfium worker thread.
pub struct PdfWorker {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    next_document_id: AtomicU64,
}

static WORKER: OnceLock<std::result::Result<PdfWorker, String>> = OnceLock::new();

impl PdfWorker {
    /// Returns the shared worker, starting it and binding Pdfium on first use.
    ///
    /// Fails if the Pdfium dynamic library cannot be found.
    pub fn global() -> Result<&'static PdfWorker> {
        WORKER
            .get_or_init(|| Self::spawn().map_err(|error| format!("{error:#}")))
            .as_ref()
            .map_err(|error| anyhow!("{error}"))
    }

    fn spawn() -> Result<Self> {
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread_queue = queue.clone();
        std::thread::Builder::new()
            .name("pdfium-worker".into())
            .spawn(move || {
                let Some(pdfium) = loader::discover_and_bind_pdfium() else {
                    ready_tx
                        .send(Err(anyhow!(
                            "Pdfium library not found. Install libpdfium \
                             (see scripts/download_pdfium.sh) or set PDFIUM_LIB_PATH."
                        )))
                        .ok();
                    return;
                };
                ready_tx.send(Ok(())).ok();
                run(&pdfium, &thread_queue);
            })
            .context("failed to spawn the Pdfium worker thread")?;
        ready_rx
            .recv()
            .context("Pdfium worker thread exited during startup")??;
        Ok(Self {
            queue,
            next_document_id: AtomicU64::new(1),
        })
    }

    /// Parses `bytes` as a PDF and keeps it open until [`PdfWorker::close`].
    pub fn open(&self, bytes: Vec<u8>, reply: impl FnOnce(Result<DocumentInfo>) + Send + 'static) {
        let id = self.next_document_id.fetch_add(1, Ordering::Relaxed);
        self.push_control(Job::Open {
            id,
            bytes,
            reply: Box::new(reply),
        });
    }

    /// Releases a document opened with [`PdfWorker::open`].
    pub fn close(&self, document: DocumentId) {
        self.push_control(Job::Close(document));
    }

    /// Queues a tile render. Dropping the returned ticket cancels it if it hasn't run yet.
    #[must_use]
    pub fn render(
        &self,
        request: TileRequest,
        reply: impl FnOnce(Result<RenderedTile>) + Send + 'static,
    ) -> RenderTicket {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (lock, condvar) = &*self.queue;
        let mut queue = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.renders.retain(|job| match job {
            Job::Render { cancelled, .. } => !cancelled.load(Ordering::Acquire),
            _ => true,
        });
        queue.renders.push(Job::Render {
            request,
            cancelled: cancelled.clone(),
            reply: Box::new(reply),
        });
        condvar.notify_one();
        RenderTicket { cancelled }
    }

    fn push_control(&self, job: Job) {
        let (lock, condvar) = &*self.queue;
        lock.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .control
            .push_back(job);
        condvar.notify_one();
    }
}

struct OpenDocument<'p> {
    // Declared before `document` so loaded pages are closed before their document.
    loaded_pages: VecDeque<(usize, PdfPage<'p>)>,
    flattened_pages: HashSet<usize>,
    has_form: bool,
    document: PdfDocument<'p>,
}

impl<'p> OpenDocument<'p> {
    fn page(&mut self, index: usize) -> Result<&mut PdfPage<'p>> {
        if let Some(position) = self.loaded_pages.iter().position(|(i, _)| *i == index) {
            let entry = self
                .loaded_pages
                .remove(position)
                .expect("position is in bounds");
            self.loaded_pages.push_front(entry);
        } else {
            let page_index = PdfPageIndex::try_from(index).context("page index out of range")?;
            let mut page = self
                .document
                .pages()
                .get(page_index)
                .with_context(|| format!("failed to load page {}", index + 1))?;
            // Tiles render through a transform matrix, which Pdfium can't combine with
            // interactive form rendering, so bake form appearances into the page contents.
            // This only touches the in-memory copy of the document.
            if self.has_form && self.flattened_pages.insert(index) {
                page.flatten().ok();
                page = self
                    .document
                    .pages()
                    .get(page_index)
                    .with_context(|| format!("failed to reload page {}", index + 1))?;
            }
            self.loaded_pages.push_front((index, page));
            self.loaded_pages.truncate(LOADED_PAGE_CACHE);
        }
        Ok(&mut self.loaded_pages[0].1)
    }
}

fn run(pdfium: &Pdfium, queue: &(Mutex<Queue>, Condvar)) {
    let mut documents: HashMap<DocumentId, OpenDocument<'_>> = HashMap::new();
    let (lock, condvar) = queue;
    loop {
        let job = {
            let mut queue = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                if let Some(job) = queue.control.pop_front() {
                    break job;
                }
                if let Some(job) = queue.renders.pop() {
                    break job;
                }
                queue = condvar
                    .wait(queue)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };

        match job {
            Job::Open { id, bytes, reply } => {
                reply(open_document(pdfium, id, bytes, &mut documents));
            }
            Job::Close(id) => {
                documents.remove(&id);
            }
            Job::Render {
                request,
                cancelled,
                reply,
            } => {
                if cancelled.load(Ordering::Acquire) {
                    continue;
                }
                let result = documents
                    .get_mut(&request.document)
                    .context("document is not open")
                    .and_then(|document| render_tile(document, &request));
                reply(result);
            }
        }
    }
}

fn open_document<'p>(
    pdfium: &'p Pdfium,
    id: DocumentId,
    bytes: Vec<u8>,
    documents: &mut HashMap<DocumentId, OpenDocument<'p>>,
) -> Result<DocumentInfo> {
    validate_pdf_bytes(&bytes)?;
    let document = pdfium
        .load_pdf_from_byte_vec(bytes, None)
        .map_err(|error| anyhow!("Pdfium could not parse the document: {error}"))?;
    let pages = document
        .pages()
        .iter()
        .map(|page| PageDimensions::new(page.width().value, page.height().value))
        .collect();
    let title = document
        .metadata()
        .get(PdfDocumentMetadataTagType::Title)
        .map(|tag| tag.value().trim().to_string())
        .filter(|title| !title.is_empty());
    let has_form = document.form().is_some();
    documents.insert(
        id,
        OpenDocument {
            loaded_pages: VecDeque::new(),
            flattened_pages: HashSet::new(),
            has_form,
            document,
        },
    );
    Ok(DocumentInfo { id, pages, title })
}

fn render_tile(document: &mut OpenDocument<'_>, request: &TileRequest) -> Result<RenderedTile> {
    let TileRequest {
        page_index,
        scale,
        x,
        y,
        width,
        height,
        ..
    } = *request;
    anyhow::ensure!(
        (1..=MAX_TILE_EDGE).contains(&width) && (1..=MAX_TILE_EDGE).contains(&height),
        "tile size {width}x{height} is out of range"
    );
    anyhow::ensure!(scale.is_finite() && scale > 0.0, "invalid scale {scale}");

    let page = document.page(page_index)?;
    let config = PdfRenderConfig::new()
        .set_fixed_size(width as i32, height as i32)
        .set_format(PdfBitmapFormat::BGRA)
        .set_reverse_byte_order(false)
        .set_clear_color(PdfColor::WHITE)
        .render_annotations(true)
        .set_text_smoothing(true)
        .set_image_smoothing(true)
        .set_path_smoothing(true)
        .transform(scale, 0.0, 0.0, scale, -(x as f32), -(y as f32))
        .map_err(|error| anyhow!("invalid tile transform: {error}"))?;
    let bitmap = page
        .render_with_config(&config)
        .map_err(|error| anyhow!("Pdfium failed to render page {}: {error}", page_index + 1))?;
    let bgra = bitmap.as_raw_bytes();
    anyhow::ensure!(
        bgra.len() == width as usize * height as usize * 4,
        "unexpected bitmap stride"
    );
    drop(bitmap);

    Ok(RenderedTile {
        width,
        height,
        bgra,
    })
}
