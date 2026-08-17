use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=CODEX_MCP_2026_FIXTURE_SOURCE");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR"))
        .join("fixture.rs");
    let body = match std::env::var_os("CODEX_MCP_2026_FIXTURE_SOURCE") {
        Some(source) => {
            let source = PathBuf::from(source)
                .canonicalize()
                .expect("Codex MCP fixture source must exist");
            println!("cargo:rerun-if-changed={}", source.display());
            std::fs::read_to_string(source).expect("Codex MCP fixture source must be UTF-8")
        }
        None => String::from(
            "fn main() {\n\
                 eprintln!(\"build through runtime/scripts/test-codex-mcp-2026-compat.sh\");\n\
                 std::process::exit(2);\n\
             }\n",
        ),
    };
    std::fs::write(output, body).expect("write generated compatibility fixture");
}
