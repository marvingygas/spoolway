//! `spoolway workspace move`: move a home-mode clone from the workspace it
//! uses now to another, taking its dispatcher folder with it — the
//! documented way out for a clone that ended up in the wrong workspace,
//! `init --workspace <other>` itself having always refused to be that (see
//! `Placement::choose_any`'s own refusal).

use super::*;

/// Move `root` from the workspace it is listed in now to `args.to`. All the
/// actual work — the two checks on live work, the lock ordering across both
/// workspaces, and the folder rename — lives in [`crate::repo::move_clone`];
/// this only reports what it did.
pub fn workspace_move(root: &Path, args: &WorkspaceMoveArgs) -> Result<()> {
    let moved = crate::repo::move_clone(root, &args.to, args.dispatcher.as_deref())?;
    println!(
        "  moved  {}  ->  {}/",
        root.display(),
        moved.home_dir().display()
    );
    Ok(())
}
