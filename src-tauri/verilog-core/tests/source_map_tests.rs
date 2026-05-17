use verilog_core::source_map::{LineCol, SourceMap, Span};

#[test]
fn ascii_offsets_resolve_correctly() {
    let mut sm = SourceMap::new();
    let src = "abc\ndef\nghij\n";
    let id = sm.intern("/a.v", src);

    assert_eq!(sm.offset_to_line_col(id, 0), LineCol { line: 1, col: 1 });
    assert_eq!(sm.offset_to_line_col(id, 2), LineCol { line: 1, col: 3 });
    assert_eq!(sm.offset_to_line_col(id, 4), LineCol { line: 2, col: 1 });
    assert_eq!(sm.offset_to_line_col(id, 8), LineCol { line: 3, col: 1 });
    assert_eq!(sm.offset_to_line_col(id, 11), LineCol { line: 3, col: 4 });
}

#[test]
fn multi_byte_utf8_column_counts_bytes_not_codepoints() {
    // Deliberately count bytes for simplicity; frontend converts to character
    // columns using CodeMirror's doc which handles UTF-16 offsets.
    let mut sm = SourceMap::new();
    // "αβγ" is 6 bytes (2 per greek lowercase letter) + \n
    let src = "αβγ\nsecond\n";
    let id = sm.intern("/u.v", src);

    // Start of file
    assert_eq!(sm.offset_to_line_col(id, 0), LineCol { line: 1, col: 1 });
    // After the first greek letter (2 bytes)
    assert_eq!(sm.offset_to_line_col(id, 2), LineCol { line: 1, col: 3 });
    // Beginning of line 2 — 6 bytes of "αβγ" + 1 byte newline = offset 7
    assert_eq!(sm.offset_to_line_col(id, 7), LineCol { line: 2, col: 1 });
    assert_eq!(sm.offset_to_line_col(id, 12), LineCol { line: 2, col: 6 });
}

#[test]
fn resolve_span_returns_full_info() {
    let mut sm = SourceMap::new();
    let src = "module m;\n  wire x;\nendmodule\n";
    let id = sm.intern("/p.v", src);
    let sp = Span::new(id, 12, 18); // "wire x"
    let info = sm.resolve(sp).expect("real span");
    assert_eq!(info.path, "/p.v");
    assert_eq!(info.file_id, id);
    assert_eq!(info.line_start, 2);
    assert_eq!(info.col_start, 3);
    assert_eq!(info.line_end, 2);
    assert_eq!(info.col_end, 9);
}

#[test]
fn dummy_span_resolves_to_none() {
    let sm = SourceMap::new();
    assert!(sm.resolve(Span::dummy()).is_none());
}

#[test]
fn intern_is_idempotent() {
    let mut sm = SourceMap::new();
    let a = sm.intern("/same.v", "x");
    let b = sm.intern("/same.v", "x");
    assert_eq!(a, b);
    assert_eq!(sm.file_id("/same.v"), Some(a));
}
