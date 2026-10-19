//! TypeScript → JavaScript by stripping types with oxc (L7.2, L7.5). Plain JS is
//! returned unchanged so error line numbers stay the model's own.

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::{ParseOptions, Parser};
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};
use std::path::Path;

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// [`to_js`] for a module, stripped once per source version (L7.2 compile-on-change
/// cache): keyed by the source hash and the novacode version, in `cache`.
pub fn cached(src: &str, cache: &Path) -> String {
    let key = format!("{:016x}-{}", fnv(src.as_bytes()), env!("CARGO_PKG_VERSION"));
    let path = cache.join(format!("{key}.js"));
    if let Ok(js) = std::fs::read_to_string(&path) {
        return js;
    }
    let js = to_js(src, true);
    let _ = std::fs::create_dir_all(cache);
    // Write then rename, so a reader never sees half a file.
    let tmp = cache.join(format!("{key}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &js).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    js
}

/// `src` as runnable JavaScript. JS that parses is returned as is; otherwise it
/// is parsed as TypeScript and printed without types (line numbers then refer to
/// the printed code: shortcut, add a source map when that confuses models).
/// A script that is neither comes back unchanged so QuickJS reports the error.
pub fn to_js(src: &str, module: bool) -> String {
    let alloc = Allocator::default();
    let opts = ParseOptions {
        allow_return_outside_function: true,
        ..ParseOptions::default()
    };
    let js = if module {
        SourceType::mjs()
    } else {
        SourceType::script()
    };
    if Parser::new(&alloc, src, js)
        .with_options(opts)
        .parse()
        .diagnostics
        .is_empty()
    {
        return src.to_string();
    }
    let ts = SourceType::ts().with_module(module).with_script(!module);
    let ret = Parser::new(&alloc, src, ts).with_options(opts).parse();
    if !ret.diagnostics.is_empty() {
        return src.to_string();
    }
    let mut program = ret.program;
    let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
    let t = Transformer::new(&alloc, Path::new("script.ts"), &TransformOptions::default())
        .build_with_scoping(scoping, &mut program);
    if !t.diagnostics.is_empty() {
        return src.to_string();
    }
    Codegen::new().build(&program).code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_types_and_keeps_js() {
        let js = "const a = 1;\nreturn a;";
        assert_eq!(to_js(js, false), js);
        let out = to_js(
            "interface P { x: number }\nconst p: P = { x: 1 };\nconst f = (n: number): string => `${n}`;\nawait f(p.x as number);",
            true,
        );
        assert!(!out.contains("interface") && !out.contains(": number"), "{out}");
        assert!(out.contains("const p = { x: 1 }"), "{out}");
        assert_eq!(to_js("let = = ;", false), "let = = ;");
    }
}
