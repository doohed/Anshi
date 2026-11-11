//! The file-explorer sidebar: one directory's listing and a selection.

use alloc::string::String;
use alloc::vec::Vec;

use anshi::{CString, DirEntry};

pub struct Sidebar {
    /// Absolute, canonical path of the directory being listed.
    pub cwd: String,
    pub entries: Vec<DirEntry>,
    pub sel: usize,
    /// First visible entry (scrolling).
    pub offset: usize,
}

/// What activating (Enter on) an entry asks the editor to do.
pub enum Activate {
    Nothing,
    OpenFile(String),
}

impl Sidebar {
    pub fn new(cwd: String) -> Self {
        Sidebar { cwd, entries: Vec::new(), sel: 0, offset: 0 }
    }

    /// Re-reads the listing. `..` comes first whenever we're not at the
    /// root, so going back up is always the top entry -- the VFS resolves
    /// `..` itself, this is only the listing convention. Returns false
    /// if the directory couldn't be read.
    pub fn load(&mut self) -> bool {
        self.sel = 0;
        self.offset = 0;
        self.entries.clear();
        if self.cwd != "/" {
            self.entries.push(DirEntry { name: "..".into(), is_dir: true });
        }
        let listing = CString::new(self.cwd.as_str()).ok().and_then(|p| anshi::read_dir(&p));
        match listing {
            Some(entries) => {
                self.entries.extend(entries);
                true
            }
            None => false,
        }
    }

    pub fn up(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.sel + 1 < self.entries.len() {
            self.sel += 1;
        }
    }

    pub fn first(&mut self) {
        self.sel = 0;
    }

    pub fn last(&mut self) {
        self.sel = self.entries.len().saturating_sub(1);
    }

    pub fn go_parent(&mut self) -> bool {
        self.cwd = parent(&self.cwd);
        self.load()
    }

    /// Descends into a directory (re-listing in place), or hands back a
    /// file's full path for the editor to open.
    pub fn activate(&mut self) -> Activate {
        let Some(entry) = self.entries.get(self.sel) else {
            return Activate::Nothing;
        };
        if !entry.is_dir {
            return Activate::OpenFile(join(&self.cwd, &entry.name));
        }
        self.cwd = if entry.name == ".." { parent(&self.cwd) } else { join(&self.cwd, &entry.name) };
        self.load();
        Activate::Nothing
    }

    /// Keeps the selection within the `rows` visible entries.
    pub fn scroll(&mut self, rows: usize) {
        let rows = rows.max(1);
        if self.sel < self.offset {
            self.offset = self.sel;
        }
        if self.sel >= self.offset + rows {
            self.offset = self.sel + 1 - rows;
        }
    }
}

/// `"/"` + `"foo"` -> `"/foo"`;  `"/a"` + `"foo"` -> `"/a/foo"`.
pub fn join(dir: &str, name: &str) -> String {
    let mut out = String::from(dir);
    if !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(name);
    out
}

/// `"/a/b"` -> `"/a"`;  `"/a"` -> `"/"`.
pub fn parent(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => String::from("/"),
        Some(i) => String::from(&path[..i]),
    }
}
