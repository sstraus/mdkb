//! Regression contracts for source languages previously omitted by discovery.
use mdkb::code::indexing::IndexFacade;

fn indexed_fixture(path: &str, source: &str) -> (tempfile::TempDir, IndexFacade) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(path), source).unwrap();
    let mut index = IndexFacade::create(root.path().join("code.sqlite")).unwrap();
    index.index_directory(root.path()).unwrap();
    (root, index)
}

fn assert_symbol(index: &IndexFacade, name: &str, path: &str, line: u32) {
    let hits = index.search_symbols(name, 100);
    assert!(
        hits.iter().any(|s| s.name.as_ref() == name
            && s.file_path.as_ref() == path
            && s.range.start_line == line),
        "{name} must be searchable at {path}:{line} (0-based): {hits:?}"
    );
}

#[test]
fn objc_file_filter_must_not_drop_functions_statics_and_methods() {
    let (_root, index) = indexed_fixture(
        "overlay.m",
        "static int statusPopover = 0;\nvoid showOverlay(void) {}\n@interface Overlay\n- (void)show;\n@end\n@implementation Overlay\n- (void)show {}\n- (void)setPosition:(int)x y:(int)y {}\n@end\n",
    );
    assert_symbol(&index, "statusPopover", "overlay.m", 0);
    assert_symbol(&index, "showOverlay", "overlay.m", 1);
    assert_symbol(&index, "show", "overlay.m", 3);
    assert_symbol(&index, "setPosition:y:", "overlay.m", 7);
    let language: String = index
        .db()
        .conn()
        .query_row(
            "SELECT language FROM code_files WHERE path = 'overlay.m'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(language, "objc");
}

#[test]
fn objc_headers_must_not_be_parsed_as_plain_c() {
    let (_root, index) = indexed_fixture(
        "overlay.h",
        "@interface Overlay\n- (void)show;\n@end\nvoid showOverlay(void);\n",
    );
    assert_symbol(&index, "show", "overlay.h", 1);
    assert_symbol(&index, "showOverlay", "overlay.h", 3);
}

#[test]
fn shell_file_filter_must_not_drop_function_definitions() {
    let (_root, index) = indexed_fixture(
        "build.sh",
        "#!/bin/bash\nrun_build() { printf ok; }\nfunction clean_build { :; }\n",
    );
    assert_symbol(&index, "run_build", "build.sh", 1);
    assert_symbol(&index, "clean_build", "build.sh", 2);
}

#[test]
fn ruby_file_filter_must_not_drop_classes_and_methods() {
    let (_root, index) = indexed_fixture(
        "tap.rb",
        "class Formula\n  def install\n  end\n  def self.version\n  end\nend\n",
    );
    assert_symbol(&index, "Formula", "tap.rb", 0);
    assert_symbol(&index, "install", "tap.rb", 1);
    assert_symbol(&index, "version", "tap.rb", 3);
}

#[test]
fn sql_file_filter_must_not_drop_schema_objects() {
    let (_root, index) = indexed_fixture(
        "schema.sql",
        "CREATE TABLE users (id INTEGER);\nCREATE VIEW active_users AS SELECT id FROM users;\nCREATE INDEX users_id ON users(id);\nCREATE TRIGGER count_users AFTER INSERT ON users BEGIN SELECT 1; END;\n",
    );
    assert_symbol(&index, "users", "schema.sql", 0);
    assert_symbol(&index, "active_users", "schema.sql", 1);
    assert_symbol(&index, "users_id", "schema.sql", 2);
    assert_symbol(&index, "count_users", "schema.sql", 3);
}

#[test]
fn sql_qualified_quoted_and_virtual_objects_must_keep_their_names() {
    let (_root, index) = indexed_fixture(
        "variants.sql",
        "CREATE TABLE IF NOT EXISTS main.accounts (id INTEGER);\nCREATE TEMP VIEW \"quoted_view\" AS SELECT 1;\nCREATE VIRTUAL TABLE docs USING fts5(body);\nCREATE UNIQUE INDEX IF NOT EXISTS main.accounts_id ON accounts(id);\n",
    );
    assert_symbol(&index, "accounts", "variants.sql", 0);
    assert_symbol(&index, "quoted_view", "variants.sql", 1);
    assert_symbol(&index, "docs", "variants.sql", 2);
    assert_symbol(&index, "accounts_id", "variants.sql", 3);
}

#[test]
fn cgo_preamble_symbols_must_keep_go_source_lines() {
    let (_root, index) = indexed_fixture(
        "bridge.go",
        "package bridge\n/*\nstatic int bridge_count = 0;\nvoid showOverlay(void) {}\n*/\nimport \"C\"\n\nfunc Show() { C.showOverlay() }\n",
    );
    assert_symbol(&index, "bridge_count", "bridge.go", 2);
    assert_symbol(&index, "showOverlay", "bridge.go", 3);
    assert_symbol(&index, "Show", "bridge.go", 7);
}

#[test]
fn recorded_cocoa_sources_keep_native_definitions_and_go_coordinates() {
    // Catches accepting .m but losing pointer-returning functions or Cocoa syntax.
    let (_root, index) = indexed_fixture(
        "notify.m",
        include_str!("fixtures/index_languages/notify.m"),
    );
    assert_symbol(&index, "controlIconTimer", "notify.m", 17);
    assert_symbol(&index, "drawRect:", "notify.m", 50);
    // Catches ignoring Objective-C cgo preambles because of #cgo directives.
    let (_root, index) = indexed_fixture(
        "pasteboard.go",
        include_str!("fixtures/index_languages/pasteboard.go"),
    );
    assert_symbol(&index, "setPasteboard", "pasteboard.go", 11);
    assert_symbol(&index, "getPasteboard", "pasteboard.go", 20);
    assert_symbol(&index, "SetClipboard", "pasteboard.go", 37);
}

#[test]
fn cgo_line_comments_are_definitions_but_detached_comments_are_not() {
    // Catches parsing arbitrary Go documentation as C and stripping source rows.
    let (_root, index) = indexed_fixture(
        "bridge.go",
        "package bridge\n// void detached(void) {}\n\n// static int line_count = 0;\n// void attached(void) {}\nimport \"C\"\n",
    );
    assert_symbol(&index, "line_count", "bridge.go", 3);
    assert_symbol(&index, "attached", "bridge.go", 4);
    assert_eq!(
        index.search_symbols("detached", 100),
        [] as [mdkb::code::symbol::Symbol; 0]
    );
}

#[test]
fn ordinary_go_comments_do_not_create_foreign_symbols() {
    // Catches treating a C-looking comment above a real import as a preamble.
    let (_root, index) = indexed_fixture(
        "plain.go",
        "package plain\n/* void imaginary(void) {} */\nimport \"fmt\"\nfunc Real() {}\n",
    );
    assert_symbol(&index, "Real", "plain.go", 3);
    assert_eq!(
        index.search_symbols("imaginary", 100),
        [] as [mdkb::code::symbol::Symbol; 0]
    );
}

#[test]
fn objc_markers_in_header_comments_and_strings_do_not_change_c() {
    // Catches a textual @interface heuristic instead of syntax-based detection.
    let (root, index) = indexed_fixture(
        "plain.h",
        "// @interface Fake\nstatic const char *marker = \"@interface String\";\n",
    );
    assert_symbol(&index, "marker", "plain.h", 1);
    let language: String = index
        .db()
        .conn()
        .query_row(
            "SELECT language FROM code_files WHERE path = 'plain.h'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(language, "c");
    let parsed = mdkb::core::code::handle_code_parse(&root.path().join("plain.h")).unwrap();
    assert!(parsed.iter().any(|s| s.name.as_ref() == "marker"));
}
