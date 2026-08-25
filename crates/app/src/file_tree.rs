//! Lazy file-tree view for egui.
//!
//! 3 fixed roots: Favorites (from settings), Recent (from settings),
//! This PC (drives). Each folder is a `TreeNode`; subfolders are
//! loaded on demand via `enumerate_subdirs`.

use std::path::{Path, PathBuf};
use parking_lot::Mutex;

/// One node in the tree. `children` is `None` until the first expand
/// (we don't want to walk deep trees eagerly on a large disk).
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub path: PathBuf,
    pub display_name: String,
    pub is_dir: bool,
    pub children: Option<Vec<TreeNode>>,
    pub loading: bool,
    /// Cached rendered text width (logical px) for content-based panel
    /// sizing. 0 = not measured yet.
    pub text_w: f32,
}

impl TreeNode {
    pub fn root(path: PathBuf, display_name: impl Into<String>) -> Self {
        Self {
            path,
            display_name: display_name.into(),
            is_dir: true,
            children: Some(Vec::new()),
            loading: false,
            text_w: 0.0,
        }
    }
}

/// Shared state for the file tree. Owned by MainWindow, accessed by the
/// draw function in `window.rs`.
pub struct FileTree {
    pub state: Mutex<TreeTreeState>,
}

pub struct TreeTreeState {
    /// Fixed roots (Favorites, Recent, This PC).
    pub roots: Vec<TreeNode>,
    /// Currently expanded paths per root (for restore on redraw).
    /// Index 0 = Favorites, 1 = Recent, 2 = This PC
    pub expanded: [std::collections::HashSet<PathBuf>; 3],
    /// When set, scroll the This PC tree to this node on the next draw
    /// (and highlight it), then clear.
    pub reveal_target: Option<PathBuf>,
}

impl FileTree {
    pub fn new() -> Self {
        let mut state = TreeTreeState {
            roots: Vec::new(),
            expanded: [
                std::collections::HashSet::new(),
                std::collections::HashSet::new(),
                std::collections::HashSet::new(),
            ],
            reveal_target: None,
        };

        // Favorites - load from settings if available
        let mut fav = TreeNode::root(PathBuf::from(""), "★ Favorites");
        fav.display_name = "Favorites".into();
        // Try to load favorite folders from settings (lazy)
        fav.loading = true;
        state.roots.push(fav);

        // Recent - load from settings if available
        let mut recent = TreeNode::root(PathBuf::from(""), "Recent");
        recent.display_name = "Recent".into();
        recent.loading = true;
        state.roots.push(recent);

        // This PC - enumerate drives
        let mut pc = TreeNode::root(PathBuf::from(""), "This PC");
        pc.children = Some(Self::enumerate_drives());
        state.roots.push(pc);

        Self { state: Mutex::new(state) }
    }

    /// Rebuild the "Recent" root's children from the settings list.
    pub fn refresh_recent(&self, folders: &[PathBuf]) {
        let mut state = self.state.lock();
        if let Some(recent) = state.roots.get_mut(1) {
            recent.children = Some(Self::leaf_nodes(folders));
        }
    }

    /// Rebuild the "Favorites" root's children from the settings list.
    pub fn refresh_favorites(&self, folders: &[PathBuf]) {
        let mut state = self.state.lock();
        if let Some(fav) = state.roots.get_mut(0) {
            fav.children = Some(Self::leaf_nodes(folders));
        }
    }

    fn leaf_nodes(folders: &[PathBuf]) -> Vec<TreeNode> {
        folders
            .iter()
            .map(|f| TreeNode {
                path: f.clone(),
                display_name: f
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| f.to_string_lossy().into_owned()),
                is_dir: true,
                children: Some(Vec::new()),
                loading: false,
                text_w: 0.0,
            })
            .collect()
    }

    /// Reveal a path inside the This PC tree: expand every ancestor level
    /// and flag the node so the next draw scrolls it into view.
    pub fn reveal(&self, path: &Path) {
        let mut state = self.state.lock();
        // Insert every ancestor prefix (drive root … parent) into the
        // This PC expansion set.
        let mut prefix: Vec<PathBuf> = Vec::new();
        for comp in path.ancestors().skip(1) {
            if comp.as_os_str().is_empty() {
                continue;
            }
            prefix.push(comp.to_path_buf());
        }
        // ancestors() yields longest → shortest; insert all.
        for p in &prefix {
            state.expanded[2].insert(p.clone());
        }
        state.reveal_target = Some(path.to_path_buf());
    }

    /// Lazy: enumerate subdirectories under `path` and replace the node's
    /// `children` with the result. Also loads roots on first access.
    pub fn load_children(node: &mut TreeNode) {
        if !node.is_dir {
            return;
        }
        // Virtual roots (Favorites / Recent / This PC with empty path)
        // load their special content instead of filesystem subdirs.
        if node.path.as_os_str().is_empty() {
            // Mark as loaded so we don't re-trigger on every paint.
            node.loading = false;
            return;
        }
        let children = enumerate_subdirs(&node.path);
        node.children = Some(children);
    }

    fn enumerate_drives() -> Vec<TreeNode> {
        // Enumerate Windows drive letters A:..Z:
        let mut out = Vec::new();
        for letter in b'A'..=b'Z' {
            let path = PathBuf::from(format!("{}:\\", letter as char));
            if path.exists() {
                let mut node = TreeNode::root(path.clone(), format!("{}:", letter as char));
                node.children = Some(Vec::new());
                out.push(node);
            }
        }
        out
    }
}

fn enumerate_subdirs(dir: &Path) -> Vec<TreeNode> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let path = ent.path();
        let Ok(ft) = ent.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        // Skip hidden (`.` prefix on Unix; on Windows, check the
        // `hidden` attribute via metadata).
        let name = match ent.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        out.push(TreeNode {
            path: path.clone(),
            display_name: name,
            is_dir: true,
            children: Some(Vec::new()),
            loading: false,
            text_w: 0.0,
        });
    }
    out.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    out
}
