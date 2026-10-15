use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Subscription, Task};
use kkpdf_zed::{DocumentInfo, PdfWorker};
use language::{DiskState, File as _};
use project::{Project, ProjectEntryId, ProjectPath};
use util::ResultExt as _;
use worktree::{File, Worktree};

/// Wait for writes to settle before reloading, so a tool rewriting the file in
/// several steps (LaTeX, Typst, exporters) produces one reload, not several.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);

pub enum PdfItemEvent {
    /// The file was renamed, moved or deleted.
    FileHandleChanged,
    /// The document was re-parsed from disk; page geometry may have changed.
    Reloaded,
}

/// A PDF file opened in the project, parsed and kept open by the Pdfium worker.
pub struct PdfItem {
    file: Arc<File>,
    document: Arc<DocumentInfo>,
    /// Increments every time the document is reloaded from disk.
    generation: u64,
    reload_task: Option<Task<()>>,
    _worktree_subscription: Subscription,
}

impl EventEmitter<PdfItemEvent> for PdfItem {}

pub fn is_pdf_path(path: &ProjectPath) -> bool {
    path.path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
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
        let loaded = worktree
            .update(cx, |worktree, cx| worktree.load_binary_file(&path.path, cx))
            .await?;
        let document = Arc::new(open_document(loaded.content).await?);

        Ok(cx.new(|cx| {
            let subscription = cx.subscribe(&worktree, Self::on_worktree_event);
            Self {
                file: loaded.file,
                document,
                generation: 0,
                reload_task: None,
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

    pub fn abs_path(&self, cx: &App) -> Option<PathBuf> {
        Some(self.file.as_local()?.abs_path(cx))
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
        let snapshot = worktree.read(cx).snapshot();
        let entry = self
            .file
            .entry_id
            .and_then(|entry_id| snapshot.entry_for_id(entry_id))
            .or_else(|| snapshot.entry_for_path(self.file.path()));

        let new_file = match entry {
            Some(entry) => File {
                disk_state: entry
                    .mtime
                    .map_or(self.file.disk_state, |mtime| DiskState::Present {
                        mtime,
                        size: entry.size,
                    }),
                is_local: true,
                entry_id: Some(entry.id),
                path: entry.path.clone(),
                worktree: worktree.clone(),
                is_private: entry.is_private,
            },
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
        let Some(local_file) = self.file.as_local() else {
            return;
        };
        let abs_path = local_file.abs_path(cx);
        let Some(fs) = self
            .file
            .worktree
            .read(cx)
            .as_local()
            .map(|worktree| worktree.fs().clone())
        else {
            return;
        };
        self.reload_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RELOAD_DEBOUNCE).await;
            // A partially written file fails to parse; keep showing the previous
            // version, the next filesystem event will retry.
            let Some(document) = async {
                let bytes = fs.load_bytes(&abs_path).await?;
                open_document(bytes).await
            }
            .await
            .with_context(|| format!("reloading {abs_path:?}"))
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
        if !is_pdf_path(path) {
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
