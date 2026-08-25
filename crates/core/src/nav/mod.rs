//! Navigation state machine — folder list, history, current index

use std::path::PathBuf;
use std::collections::VecDeque;
use anyhow::Result;
use crate::model::ImageItem;
use crate::fs::enumerate_images;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationDirection {
    Next,
    Previous,
    First,
    Last,
    Index(usize),
}

pub struct NavigationService {
    current_folder: Option<PathBuf>,
    items: Vec<ImageItem>,
    current_index: usize,
    history: VecDeque<(PathBuf, usize)>, // (folder, index)
    max_history: usize,
}

impl NavigationService {
    pub fn new() -> Self {
        Self {
            current_folder: None,
            items: Vec::new(),
            current_index: 0,
            history: VecDeque::with_capacity(100),
            max_history: 100,
        }
    }

    pub fn navigate_folder(&mut self, folder: PathBuf) -> Result<()> {
        if self.current_folder.as_ref() == Some(&folder) {
            return Ok(());
        }
        if let Some(ref old) = self.current_folder {
            self.history.push_back((old.clone(), self.current_index));
            if self.history.len() > self.max_history { self.history.pop_front(); }
        }
        let items = enumerate_images(&folder)?
            .into_iter()
            .map(ImageItem::from_path)
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        self.current_folder = Some(folder);
        self.items = items;
        self.current_index = 0;
        Ok(())
    }

    pub fn move_to(&mut self, dir: NavigationDirection) -> Option<&ImageItem> {
        if self.items.is_empty() { return None; }
        let len = self.items.len();
        self.current_index = match dir {
            NavigationDirection::Next => (self.current_index + 1) % len,
            NavigationDirection::Previous => (self.current_index + len - 1) % len,
            NavigationDirection::First => 0,
            NavigationDirection::Last => len - 1,
            NavigationDirection::Index(i) => i.min(len - 1),
        };
        Some(&self.items[self.current_index])
    }

    /// Move to a specific index. No-op if `i` is out of range.
    pub fn set_index(&mut self, i: usize) -> Option<&ImageItem> {
        if self.items.is_empty() { return None; }
        if i >= self.items.len() { return None; }
        self.current_index = i;
        Some(&self.items[i])
    }

    pub fn current(&self) -> Option<&ImageItem> {
        self.items.get(self.current_index)
    }

    pub fn current_index(&self) -> usize { self.current_index }
    pub fn items(&self) -> &[ImageItem] { &self.items }
    pub fn folder(&self) -> Option<&PathBuf> { self.current_folder.as_ref() }
    pub fn count(&self) -> usize { self.items.len() }
    pub fn is_empty(&self) -> bool { self.items.is_empty() }
}