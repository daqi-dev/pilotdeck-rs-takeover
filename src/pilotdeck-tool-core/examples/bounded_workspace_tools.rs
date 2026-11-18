//! Minimal usage of the pure tool-core APIs: schema validation, a
//! `todo_write` update, and a bounded workspace file read.
//!
//! Run with `cargo run -p pilotdeck-tool-core --example bounded_workspace_tools`
//! (the crate prints one numbered line and exits 0).
#![forbid(unsafe_code)]

use pilotdeck_protocol::parse_json;
use pilotdeck_tool_core::{
    ReadFileRequest, WorkspaceBoundary, apply_todo_update, parse_todo_write_request,
    read_file, todo_definition, validate_tool_input,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Validate a model-supplied input against the builtin todo_write schema.
    let schema = todo_definition()
        .get("inputSchema")
        .cloned()
        .expect("todo_write definition carries its input schema");
    let input = parse_json(
        r#"{"todos":[{"id":"1","content":"read the docs","status":"in_progress"}]}"#,
    )?;
    validate_tool_input(&schema, &input)?;

    // 2) Apply the pure todo update.
    let request = parse_todo_write_request(&input)?;
    let items = apply_todo_update(&[], &request)?;
    assert_eq!(items[0].status, "in_progress");

    // 3) Bounded read of a file inside a strict workspace boundary.
    let root = std::env::temp_dir().join("pilotdeck-tool-core-example");
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("notes.txt"), b"hello from tool-core")?;
    let boundary = WorkspaceBoundary::new(&root)?;
    let result = read_file(&boundary, &ReadFileRequest::new("notes.txt"))?;
    println!("{}", result.content);

    Ok(())
}
