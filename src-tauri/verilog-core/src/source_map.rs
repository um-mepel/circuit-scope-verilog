//! Source-location tracking for the time-synced debugger.
//!
//! [`Span`] records a half-open byte range in one source file identified by a [`FileId`].
//! [`SourceMap`] owns the file paths and precomputes line-start offsets so the frontend
//! can convert a byte range into `(line, col)` positions cheaply.

use std::collections::HashMap;

/// Opaque handle to a source file registered in a [`SourceMap`].
pub type FileId = u16;

/// Sentinel [`FileId`] used for spans synthesized internally (e.g. generate-loop unrolled
/// statements when we cannot attribute them to a specific file).
pub const SYNTHETIC_FILE: FileId = u16::MAX;

/// A half-open byte range `[start, end)` inside the file identified by `file_id`.
///
/// Uses `u32` offsets since Verilog files are rarely >4 GB; keeps `Span` at 8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub file_id: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(file_id: FileId, start: u32, end: u32) -> Self {
        Self {
            file_id,
            start,
            end,
        }
    }

    /// A dummy span used when no source location is known; resolves to an empty range
    /// in the synthetic file.
    pub const fn dummy() -> Self {
        Self {
            file_id: SYNTHETIC_FILE,
            start: 0,
            end: 0,
        }
    }

    pub fn is_dummy(&self) -> bool {
        self.file_id == SYNTHETIC_FILE
    }
}

/// 1-indexed line/column pair resolved against a [`SourceMap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// Detailed span info for frontend consumers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpanInfo {
    pub file_id: FileId,
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub line_start: u32,
    pub col_start: u32,
    pub line_end: u32,
    pub col_end: u32,
}

/// Registry of source files and precomputed line offsets.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    paths: Vec<String>,
    /// For each file, the byte offset where each 1-indexed line begins.
    /// `line_starts[file_id][0]` is always 0; `line_starts[file_id].len()` = line count + 1.
    line_starts: Vec<Vec<u32>>,
    path_to_id: HashMap<String, FileId>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern the given path and precompute its line-start offsets. If `path` was
    /// already registered, returns the existing id and does not re-scan.
    pub fn intern(&mut self, path: &str, content: &str) -> FileId {
        if let Some(&id) = self.path_to_id.get(path) {
            return id;
        }
        let id: FileId = self
            .paths
            .len()
            .try_into()
            .expect("more than u16::MAX source files");
        debug_assert!(id != SYNTHETIC_FILE, "file-id overflow");
        let mut starts: Vec<u32> = Vec::with_capacity(content.len() / 40 + 1);
        starts.push(0);
        let bytes = content.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                starts.push((i as u32) + 1);
            }
        }
        self.paths.push(path.to_string());
        self.line_starts.push(starts);
        self.path_to_id.insert(path.to_string(), id);
        id
    }

    pub fn path(&self, id: FileId) -> Option<&str> {
        if id == SYNTHETIC_FILE {
            return None;
        }
        self.paths.get(id as usize).map(String::as_str)
    }

    pub fn file_id(&self, path: &str) -> Option<FileId> {
        self.path_to_id.get(path).copied()
    }

    pub fn files(&self) -> impl Iterator<Item = (FileId, &str)> {
        self.paths
            .iter()
            .enumerate()
            .map(|(i, p)| (i as FileId, p.as_str()))
    }

    /// Convert a byte offset into a 1-indexed `(line, col)` using binary search.
    pub fn offset_to_line_col(&self, file_id: FileId, offset: u32) -> LineCol {
        if file_id == SYNTHETIC_FILE {
            return LineCol { line: 1, col: 1 };
        }
        let Some(starts) = self.line_starts.get(file_id as usize) else {
            return LineCol { line: 1, col: 1 };
        };
        if starts.is_empty() {
            return LineCol { line: 1, col: 1 };
        }
        let idx = match starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line = (idx as u32) + 1;
        let col = offset.saturating_sub(starts[idx]) + 1;
        LineCol { line, col }
    }

    /// Resolve a span to line/column pairs. Returns `None` when the span points
    /// at the synthetic file.
    pub fn resolve(&self, span: Span) -> Option<SpanInfo> {
        if span.is_dummy() {
            return None;
        }
        let path = self.path(span.file_id)?.to_string();
        let start = self.offset_to_line_col(span.file_id, span.start);
        let end = self.offset_to_line_col(span.file_id, span.end);
        Some(SpanInfo {
            file_id: span.file_id,
            path,
            start: span.start,
            end: span.end,
            line_start: start.line,
            col_start: start.col,
            line_end: end.line,
            col_end: end.col,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_then_line_col() {
        let mut sm = SourceMap::new();
        let src = "line1\nline two\nthird\n";
        let id = sm.intern("/tmp/x.v", src);
        assert_eq!(id, 0);
        assert_eq!(
            sm.offset_to_line_col(id, 0),
            LineCol { line: 1, col: 1 }
        );
        assert_eq!(
            sm.offset_to_line_col(id, 6),
            LineCol { line: 2, col: 1 }
        );
        assert_eq!(
            sm.offset_to_line_col(id, 10),
            LineCol { line: 2, col: 5 }
        );
        assert_eq!(
            sm.offset_to_line_col(id, 15),
            LineCol { line: 3, col: 1 }
        );
        assert_eq!(
            sm.offset_to_line_col(id, 16),
            LineCol { line: 3, col: 2 }
        );
    }

    #[test]
    fn intern_deduplicates() {
        let mut sm = SourceMap::new();
        let a = sm.intern("/a.v", "x");
        let b = sm.intern("/a.v", "x");
        assert_eq!(a, b);
    }

    #[test]
    fn resolve_gives_info_for_real_file() {
        let mut sm = SourceMap::new();
        let src = "module foo;\nendmodule\n";
        let id = sm.intern("/m.v", src);
        let sp = Span::new(id, 7, 10); // "foo" in "module foo;"
        let info = sm.resolve(sp).unwrap();
        assert_eq!(info.path, "/m.v");
        assert_eq!(info.line_start, 1);
        assert_eq!(info.col_start, 8);
        assert_eq!(info.line_end, 1);
        assert_eq!(info.col_end, 11);
    }

    #[test]
    fn dummy_span_resolves_to_none() {
        let sm = SourceMap::new();
        assert!(sm.resolve(Span::dummy()).is_none());
    }
}
