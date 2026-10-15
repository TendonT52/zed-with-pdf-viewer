use std::path::PathBuf;

use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use workspace::{ItemId, WorkspaceDb, WorkspaceId};

pub struct PdfViewerDb(ThreadSafeConnection);

impl Domain for PdfViewerDb {
    const NAME: &str = stringify!(PdfViewerDb);

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE pdf_viewers (
            workspace_id INTEGER,
            item_id INTEGER UNIQUE,

            pdf_path BLOB,
            view_state TEXT,

            PRIMARY KEY(workspace_id, item_id),
            FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
            ON DELETE CASCADE
        ) STRICT;
    )];
}

db::static_connection!(PdfViewerDb, [WorkspaceDb]);

impl PdfViewerDb {
    query! {
        pub async fn save_pdf(
            item_id: ItemId,
            workspace_id: WorkspaceId,
            pdf_path: PathBuf,
            view_state: String
        ) -> Result<()> {
            INSERT OR REPLACE INTO pdf_viewers(item_id, workspace_id, pdf_path, view_state)
            VALUES (?, ?, ?, ?)
        }
    }

    query! {
        pub fn get_pdf(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<(PathBuf, String)>> {
            SELECT pdf_path, view_state
            FROM pdf_viewers
            WHERE item_id = ? AND workspace_id = ?
        }
    }
}
