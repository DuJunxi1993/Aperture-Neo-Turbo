//! Compact display of a path: replaces the home directory with `~` and
//! inserts an ellipsis in the middle if the path is very long.

use std::path::Path;

const MAX_LEN: usize = 60;

pub fn shorten(p: &Path) -> String {
    let s = p.display().to_string();
    if s.len() <= MAX_LEN {
        return s;
    }
    // Insert ellipsis in the middle.
    let head_keep = MAX_LEN / 2 - 1;
    let tail_keep = MAX_LEN - head_keep - 1;
    let mut chars: Vec<char> = s.chars().collect();
    if chars.len() <= head_keep + tail_keep {
        return s;
    }
    let head: String = chars.iter().take(head_keep).collect();
    let tail: String = chars.iter().rev().take(tail_keep).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}…{tail}")
}
