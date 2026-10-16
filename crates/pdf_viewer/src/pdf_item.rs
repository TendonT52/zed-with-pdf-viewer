use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use futures::{FutureExt as _, channel::oneshot, future::Shared};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Subscription, Task, WeakEntity,
};
use kkpdf_zed::{DocumentInfo, PdfWorker};
use language::{DiskState, File as _};
use project::{Project, ProjectEntryId, ProjectPath};
use util::{ResultExt as _, rel_path::RelPath};
use worktree::{Entry, File, LoadedBinaryFile, Worktree};

/// Wait for writes to settle before reloading, so a tool rewriting the file in
/// several steps (LaTeX, Typst, exporters) produces one reload, not several.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);

/// How often to check whether a remote download has been written to disk.
const DOWNLOAD_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long to wait for a remote download to be written to disk once the remote
/// host has sent every byte.
const DOWNLOAD_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How many polls a superseded download's size must stay the same before it
/// counts as written.
const SUPERSEDED_DOWNLOAD_STABLE_POLLS: u32 = 10;

pub enum PdfItemEvent {
    /// The file was renamed, moved or deleted.
    FileHandleChanged,
    /// The document was re-parsed from disk; page geometry may have changed.
    Reloaded,
}

/// A PDF file opened in the project, parsed and kept open by the Pdfium worker.
pub struct PdfItem {
    project: WeakEntity<Project>,
    file: Arc<File>,
    document: Arc<DocumentInfo>,
    /// Increments every time the document is reloaded from disk.
    generation: u64,
    reload_task: Option<Task<()>>,
    /// The most recent read of the file for a reload. It outlives a superseded
    /// reload so the next one can wait for it: the project tracks remote
    /// downloads by path, so two downloads of one file must never overlap.
    latest_read: Option<Shared<Task<()>>>,
    _worktree_subscription: Subscription,
}

impl EventEmitter<PdfItemEvent> for PdfItem {}

/// Whether `path` names a PDF file.
///
/// When a single file is opened on its own (from Finder or `zed file.pdf`), the file
/// is the worktree root and its project path is empty, so the extension has to come
/// from the worktree's absolute path instead.
pub fn is_pdf_file(project: &Entity<Project>, path: &ProjectPath, cx: &App) -> bool {
    let extension = path.path.extension().map(str::to_owned).or_else(|| {
        let worktree = project.read(cx).worktree_for_id(path.worktree_id, cx)?;
        let abs_path = worktree.read(cx).abs_path();
        Some(abs_path.extension()?.to_str()?.to_owned())
    });
    extension.is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

/// Parses `bytes` in the Pdfium worker.
pub async fn open_document(bytes: Vec<u8>) -> Result<DocumentInfo> {
    let worker = PdfWorker::global()?;
    let (tx, rx) = oneshot::channel();
    worker.open(bytes, move |result| {
        tx.send(result).ok();
    });
    rx.await.context("the Pdfium worker stopped")?
}

fn file_for_entry(
    entry: &Entry,
    worktree: Entity<Worktree>,
    is_local: bool,
    fallback_disk_state: DiskState,
) -> File {
    File {
        disk_state: entry
            .mtime
            .map_or(fallback_disk_state, |mtime| DiskState::Present {
                mtime,
                size: entry.size,
            }),
        is_local,
        entry_id: Some(entry.id),
        path: entry.path.clone(),
        worktree,
        is_private: entry.is_private,
    }
}

/// Reads the file's bytes: from disk in local worktrees, downloaded from the
/// remote host otherwise.
async fn read_file(
    project: WeakEntity<Project>,
    worktree: Entity<Worktree>,
    path: Arc<RelPath>,
    cx: &mut AsyncApp,
) -> Result<LoadedBinaryFile> {
    if worktree.read_with(cx, |worktree, _| worktree.is_local()) {
        return worktree
            .update(cx, |worktree, cx| worktree.load_binary_file(&path, cx))
            .await;
    }

    let entry = worktree
        .read_with(cx, |worktree, _| worktree.entry_for_path(&path).cloned())
        .with_context(|| format!("{} not found on the remote host", path.as_unix_str()))?;
    let file = Arc::new(file_for_entry(
        &entry,
        worktree.clone(),
        false,
        DiskState::New,
    ));
    let content = download_remote_file(project, &worktree, path, cx).await?;
    Ok(LoadedBinaryFile { file, content })
}

fn remote_file_size(worktree: &Entity<Worktree>, path: &RelPath, cx: &AsyncApp) -> Result<u64> {
    let size = worktree
        .read_with(cx, |worktree, _| {
            worktree.entry_for_path(path).map(|entry| entry.size)
        })
        .with_context(|| format!("{} not found on the remote host", path.as_unix_str()))?;
    // Fail early: an empty file isn't a valid PDF, and waiting for the project to
    // write an empty download would only time out.
    anyhow::ensure!(size > 0, "{} is empty", path.as_unix_str());
    Ok(size)
}

async fn download_remote_file(
    project: WeakEntity<Project>,
    worktree: &Entity<Worktree>,
    path: Arc<RelPath>,
    cx: &mut AsyncApp,
) -> Result<Vec<u8>> {
    let size_when_requested = remote_file_size(worktree, &path, cx)?;
    let directory = cx
        .background_spawn(async { tempfile::tempdir() })
        .await
        .context("creating a temporary directory for the download")?;
    let destination = directory.path().join("download.pdf");
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    project
        .update(cx, |project, cx| {
            project.download_file(worktree_id, path.clone(), destination.clone(), cx)
        })?
        .await
        .with_context(|| format!("downloading {} from the remote host", path.as_unix_str()))?;

    // The project writes the destination in its own message handler, which can
    // finish after the download request does, and only logs write errors. The
    // file is complete once it has the size the remote host reports. If the file
    // changed on the host meanwhile, that size may belong to a newer version than
    // the one downloaded; then accept the download once its size stops changing,
    // the reload that change triggers fetches the newer version.
    let attempts = DOWNLOAD_WRITE_TIMEOUT.as_millis() / DOWNLOAD_POLL_INTERVAL.as_millis();
    let mut previous_size = None;
    let mut unchanged_polls = 0;
    let mut complete = false;
    for _ in 0..attempts {
        let expected_size = remote_file_size(worktree, &path, cx)?;
        let size = std::fs::metadata(&destination)
            .ok()
            .map(|metadata| metadata.len());
        if size == Some(expected_size) {
            complete = true;
            break;
        }
        if size.is_some() && size == previous_size {
            unchanged_polls += 1;
        } else {
            unchanged_polls = 0;
        }
        if expected_size != size_when_requested
            && size.is_some_and(|size| size > 0)
            && unchanged_polls >= SUPERSEDED_DOWNLOAD_STABLE_POLLS
        {
            complete = true;
            break;
        }
        previous_size = size;
        cx.background_executor().timer(DOWNLOAD_POLL_INTERVAL).await;
    }
    anyhow::ensure!(
        complete,
        "timed out downloading {} from the remote host",
        path.as_unix_str()
    );

    cx.background_spawn(async move {
        let content = std::fs::read(&destination)
            .with_context(|| format!("reading the downloaded file {destination:?}"));
        directory
            .close()
            .context("removing the download's temporary directory")
            .log_err();
        content
    })
    .await
}

impl PdfItem {
    async fn load(
        project: Entity<Project>,
        path: ProjectPath,
        cx: &mut AsyncApp,
    ) -> Result<Entity<Self>> {
        let worktree = project
            .read_with(cx, |project, cx| {
                project.worktree_for_id(path.worktree_id, cx)
            })
            .context("worktree not found")?;
        let loaded = read_file(project.downgrade(), worktree.clone(), path.path, cx).await?;
        let document = Arc::new(open_document(loaded.content).await?);

        Ok(cx.new(|cx| {
            let subscription = cx.subscribe(&worktree, Self::on_worktree_event);
            Self {
                project: project.downgrade(),
                file: loaded.file,
                document,
                generation: 0,
                reload_task: None,
                latest_read: None,
                _worktree_subscription: subscription,
            }
        }))
    }

    pub fn document(&self) -> &Arc<DocumentInfo> {
        &self.document
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn file(&self) -> &Arc<File> {
        &self.file
    }

    /// The file's absolute path on this computer, or `None` in remote projects.
    pub fn abs_path(&self, cx: &App) -> Option<PathBuf> {
        Some(self.file.as_local()?.abs_path(cx))
    }

    /// The file's absolute path on the machine that has it: this computer in
    /// local projects, the remote host in remote ones. `open_abs_path` opens it
    /// again in the same project.
    pub fn path_on_host(&self, cx: &App) -> PathBuf {
        self.file.worktree.read(cx).absolutize(&self.file.path)
    }

    /// Opens the PDF at `abs_path`, a path from `path_on_host`, in `project`.
    pub async fn open_abs_path(
        project: Entity<Project>,
        abs_path: PathBuf,
        cx: &mut AsyncApp,
    ) -> Result<Entity<Self>> {
        let (worktree, relative_path) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(abs_path, false, cx)
            })
            .await
            .context("path not found")?;
        let project_path = ProjectPath {
            worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
            path: relative_path,
        };
        cx.update(|cx| <Self as project::ProjectItem>::try_open(&project, &project_path, cx))
            .context("not a PDF file")?
            .await
    }

    pub fn project_path(&self, cx: &App) -> ProjectPath {
        ProjectPath {
            worktree_id: self.file.worktree_id(cx),
            path: self.file.path().clone(),
        }
    }

    pub fn is_deleted(&self) -> bool {
        self.file.disk_state().is_deleted()
    }

    fn on_worktree_event(
        &mut self,
        worktree: Entity<Worktree>,
        event: &worktree::Event,
        cx: &mut Context<Self>,
    ) {
        let worktree::Event::UpdatedEntries(changes) = event else {
            return;
        };
        let affects_us = changes.iter().any(|(path, entry_id, _)| {
            Some(*entry_id) == self.file.entry_id || path == self.file.path()
        });
        if affects_us {
            self.refresh_file(&worktree, cx);
        }
    }

    fn refresh_file(&mut self, worktree: &Entity<Worktree>, cx: &mut Context<Self>) {
        let worktree_ref = worktree.read(cx);
        let entry = self
            .file
            .entry_id
            .and_then(|entry_id| worktree_ref.entry_for_id(entry_id))
            .or_else(|| worktree_ref.entry_for_path(self.file.path()));

        let new_file = match entry {
            Some(entry) => file_for_entry(
                entry,
                worktree.clone(),
                worktree_ref.is_local(),
                self.file.disk_state,
            ),
            None => File {
                disk_state: DiskState::Deleted,
                ..(*self.file).clone()
            },
        };
        if new_file == *self.file {
            return;
        }

        let content_changed = new_file.disk_state != self.file.disk_state
            && matches!(new_file.disk_state, DiskState::Present { .. });
        self.file = Arc::new(new_file);
        cx.emit(PdfItemEvent::FileHandleChanged);
        cx.notify();
        if content_changed {
            self.schedule_reload(cx);
        }
    }

    fn schedule_reload(&mut self, cx: &mut Context<Self>) {
        let previous_read = self.latest_read.clone();
        self.reload_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RELOAD_DEBOUNCE).await;
            if let Some(previous_read) = previous_read {
                previous_read.await;
                // The project forgets a finished download only after writing it,
                // so give it a moment before downloading the same path again.
                cx.background_executor().timer(DOWNLOAD_POLL_INTERVAL).await;
            }

            let Ok((bytes, path)) = this.update(cx, |this, cx| {
                let (bytes_tx, bytes_rx) = oneshot::channel();
                let read = cx.spawn({
                    let project = this.project.clone();
                    let worktree = this.file.worktree.clone();
                    let path = this.file.path.clone();
                    async move |_, cx| {
                        let result = read_file(project, worktree, path, cx).await;
                        bytes_tx.send(result).ok();
                    }
                });
                this.latest_read = Some(read.shared());
                (bytes_rx, this.file.path.clone())
            }) else {
                return;
            };

            // A partially written file fails to parse; keep showing the previous
            // version, the next filesystem event will retry.
            let Some(document) = async {
                let loaded = bytes.await.context("the read was cancelled")??;
                open_document(loaded.content).await
            }
            .await
            .with_context(|| format!("reloading {}", path.as_unix_str()))
            .log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                let previous = std::mem::replace(&mut this.document, Arc::new(document));
                this.generation += 1;
                if let Ok(worker) = PdfWorker::global() {
                    worker.close(previous.id);
                }
                cx.emit(PdfItemEvent::Reloaded);
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Drop for PdfItem {
    fn drop(&mut self) {
        if let Ok(worker) = PdfWorker::global() {
            worker.close(self.document.id);
        }
    }
}

impl project::ProjectItem for PdfItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if !is_pdf_file(project, path, cx) {
            return None;
        }
        let project = project.clone();
        let path = path.clone();
        Some(cx.spawn(async move |cx| Self::load(project, path, cx).await))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.file.entry_id
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(self.project_path(cx))
    }

    fn is_dirty(&self) -> bool {
        false
    }
}
