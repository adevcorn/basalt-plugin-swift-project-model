//! Tier-0 project-model plugin for Swift workspaces.
//!
//! Emits the shared versioned generic project-model JSON schema under
//! `project-model:swift` for SwiftPM, Xcode project, and Xcode workspace roots.

use basalt_plugin_sdk::prelude::*;

// ── Plugin identity & metadata ────────────────────────────────────────────────

basalt_plugin_meta! {
    name:         "swift-project-model",
    version:      "0.1.0",
    hook_flags:   CAP_PROJECT_MODEL,
    provides:     "project-model:swift",
    requires:     "",
    file_globs:   "",
    activates_on: "Package.swift\n*.xcodeproj\n*.xcworkspace\n**/Package.swift\n**/*.xcodeproj\n**/*.xcworkspace",
    activation_events: "",
}

// ── Target kind constants ─────────────────────────────────────────────────────

const TARGET_KIND_UNKNOWN: u8 = 0;
const TARGET_KIND_LIBRARY: u8 = 1;
const TARGET_KIND_BINARY: u8 = 2;
const TARGET_KIND_TEST: u8 = 3;
#[allow(dead_code)]
const TARGET_KIND_WEBAPP: u8 = 4;
#[allow(dead_code)]
const TARGET_KIND_WEBAPI: u8 = 5;
const TARGET_KIND_PROC_MACRO: u8 = 6;
const TARGET_KIND_PLUGIN: u8 = 7;
const TARGET_KIND_FRAMEWORK: u8 = 8;
const TARGET_KIND_EXTENSION: u8 = 9;
#[allow(dead_code)]
const TARGET_KIND_EXAMPLE: u8 = 10;
#[allow(dead_code)]
const TARGET_KIND_BENCHMARK: u8 = 11;
#[allow(dead_code)]
const TARGET_KIND_WORKER: u8 = 12;
#[allow(dead_code)]
const TARGET_KIND_AGGREGATOR: u8 = 13;

// ── Host import ───────────────────────────────────────────────────────────────

extern "C" {
    fn basalt_read_file(path_ptr: i32, path_len: i32, out_ptr: i32, out_cap: i32) -> i32;
}

// ── Static scratch buffers ────────────────────────────────────────────────────

const FILE_BUF_SIZE: usize = 4 * 1024 * 1024;

static mut FILE_BUF: [u8; FILE_BUF_SIZE] = [0u8; FILE_BUF_SIZE];
static mut PATH_BUF: [u8; 2048] = [0u8; 2048];

// ── Path building helpers ─────────────────────────────────────────────────────

fn join_path(root: &[u8], suffix: &[u8]) -> &'static [u8] {
    let total = root.len() + 1 + suffix.len();
    if total >= 2048 {
        return &[];
    }
    let buf: &mut [u8; 2048] = unsafe { &mut *core::ptr::addr_of_mut!(PATH_BUF) };
    buf[..root.len()].copy_from_slice(root);
    buf[root.len()] = b'/';
    buf[root.len() + 1..total].copy_from_slice(suffix);
    unsafe { core::slice::from_raw_parts(buf.as_ptr(), total) }
}

// ── Byte-scan helpers ─────────────────────────────────────────────────────────

#[inline]
fn starts_with_at(haystack: &[u8], pos: usize, needle: &[u8]) -> bool {
    let end = pos + needle.len();
    end <= haystack.len() && &haystack[pos..end] == needle
}

fn find_from(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from + needle.len() > haystack.len() {
        return None;
    }
    let limit = haystack.len() - needle.len();
    for i in from..=limit {
        if starts_with_at(haystack, i, needle) {
            return Some(i);
        }
    }
    None
}

fn skip_ws(data: &[u8], mut pos: usize) -> usize {
    while pos < data.len() && matches!(data[pos], b' ' | b'\t' | b'\n' | b'\r') {
        pos += 1;
    }
    pos
}

fn read_quoted<'a>(data: &'a [u8], pos: usize) -> &'a [u8] {
    if pos >= data.len() || data[pos] != b'"' {
        return &[];
    }
    let start = pos + 1;
    let mut end = start;
    while end < data.len() && data[end] != b'"' {
        end += 1;
    }
    &data[start..end]
}

fn read_file(path: &[u8]) -> &'static [u8] {
    let buf: &mut [u8; FILE_BUF_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(FILE_BUF) };
    let n = unsafe {
        basalt_read_file(
            path.as_ptr() as i32,
            path.len() as i32,
            buf.as_mut_ptr() as i32,
            FILE_BUF_SIZE as i32,
        )
    };
    if n <= 0 {
        return &[];
    }
    unsafe { core::slice::from_raw_parts(buf.as_ptr(), n as usize) }
}

fn utf8_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn escape_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                let n = c as u32;
                out.push_str("\\u");
                out.push(char::from(b"0123456789ABCDEF"[((n >> 12) & 0xf) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[((n >> 8) & 0xf) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[((n >> 4) & 0xf) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[(n & 0xf) as usize]));
            }
            c => out.push(c),
        }
    }
    out
}

fn sanitize_id(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "swift-project".to_string();
    }
    trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn owner_json(path: &str, match_kind: &str, label: &str, project_id: &str, kind: &str) -> String {
    format!(
        "{{\"path\":\"{}\",\"match_kind\":\"{}\",\"label\":\"{}\",\"project_id\":\"{}\",\"target_id\":\"{}\",\"kind\":\"{}\"}}",
        escape_json(path),
        escape_json(match_kind),
        escape_json(label),
        escape_json(project_id),
        escape_json(project_id),
        escape_json(kind),
    )
}

// ── Target extraction ─────────────────────────────────────────────────────────

const MAX_TARGETS: usize = 64;

struct Targets {
    names: [[u8; 128]; MAX_TARGETS],
    lens: [usize; MAX_TARGETS],
    kinds: [u8; MAX_TARGETS],
    subtypes: [[u8; 32]; MAX_TARGETS],
    subtype_lens: [usize; MAX_TARGETS],
    count: usize,
}

impl Targets {
    fn new() -> Self {
        Self {
            names: [[0u8; 128]; MAX_TARGETS],
            lens: [0; MAX_TARGETS],
            kinds: [TARGET_KIND_UNKNOWN; MAX_TARGETS],
            subtypes: [[0u8; 32]; MAX_TARGETS],
            subtype_lens: [0; MAX_TARGETS],
            count: 0,
        }
    }

    fn push(&mut self, name: &[u8], kind: u8, subtype: &[u8]) {
        if self.count >= MAX_TARGETS {
            return;
        }
        let len = name.len().min(128);
        self.names[self.count][..len].copy_from_slice(&name[..len]);
        self.lens[self.count] = len;
        self.kinds[self.count] = kind;
        let subtype_len = subtype.len().min(32);
        self.subtypes[self.count][..subtype_len].copy_from_slice(&subtype[..subtype_len]);
        self.subtype_lens[self.count] = subtype_len;
        self.count += 1;
    }

    fn name(&self, i: usize) -> &[u8] {
        &self.names[i][..self.lens[i]]
    }

    fn subtype(&self, i: usize) -> &[u8] {
        &self.subtypes[i][..self.subtype_lens[i]]
    }
}

// ── SPM parsing ───────────────────────────────────────────────────────────────

const SPM_KEYWORDS: &[(&[u8], u8, &[u8])] = &[
    (b".executableTarget(", TARGET_KIND_BINARY, b"swift-executable"),
    (b".testTarget(", TARGET_KIND_TEST, b"swiftpm-test"),
    (b".macro(", TARGET_KIND_PROC_MACRO, b"swift-macro"),
    (b".plugin(", TARGET_KIND_PLUGIN, b"swift-plugin"),
    (b".binaryTarget(", TARGET_KIND_FRAMEWORK, b"binary-target"),
    (b".target(", TARGET_KIND_LIBRARY, b"swift-library"),
];

fn parse_package_swift(data: &[u8]) -> Targets {
    let mut targets = Targets::new();
    let mut pos = 0;
    while pos < data.len() {
        let mut found: Option<(usize, u8, &[u8])> = None;
        for &(kw, kind, subtype) in SPM_KEYWORDS {
            if let Some(p) = find_from(data, pos, kw) {
                match found {
                    None => found = Some((p, kind, subtype)),
                    Some((q, _, _)) if p < q => found = Some((p, kind, subtype)),
                    _ => {}
                }
            }
        }
        let (kw_pos, kind, subtype) = match found {
            Some(f) => f,
            None => break,
        };
        let after_paren = match find_from(data, kw_pos, b"(") {
            Some(p) => p + 1,
            None => {
                pos = kw_pos + 1;
                continue;
            }
        };
        let window_end = (after_paren + 256).min(data.len());
        let name_kw = match find_from(&data[..window_end], after_paren, b"name:") {
            Some(p) => p,
            None => {
                pos = kw_pos + 1;
                continue;
            }
        };
        let after_colon = skip_ws(data, name_kw + 5);
        let name = read_quoted(data, after_colon);
        if !name.is_empty() {
            targets.push(name, kind, subtype);
        }
        pos = kw_pos + 1;
    }
    targets
}

fn parse_package_name(data: &[u8]) -> Option<String> {
    let key = b"name:";
    let mut pos = 0;
    while let Some(found) = find_from(data, pos, key) {
        let start = skip_ws(data, found + key.len());
        let value = read_quoted(data, start);
        if !value.is_empty() {
            return Some(utf8_lossy(value));
        }
        pos = found + key.len();
    }
    None
}

// ── Xcode parsing ─────────────────────────────────────────────────────────────

fn product_type_info(product_type: &[u8]) -> (u8, &'static [u8]) {
    const PREFIX: &[u8] = b"com.apple.product-type.";
    let s = if product_type.len() > PREFIX.len() && &product_type[..PREFIX.len()] == PREFIX {
        &product_type[PREFIX.len()..]
    } else {
        product_type
    };
    if s == b"application" {
        (TARGET_KIND_BINARY, b"xcode-app")
    } else if s == b"tool" || s == b"commandline-tool" {
        (TARGET_KIND_BINARY, b"cli")
    } else if s == b"app-clip" {
        (TARGET_KIND_BINARY, b"app-clip")
    } else if s == b"framework" || s == b"static-framework" {
        (TARGET_KIND_FRAMEWORK, b"framework")
    } else if s == b"unit-test-bundle" || s == b"ui-testing-bundle" {
        (TARGET_KIND_TEST, b"xcode-test-bundle")
    } else if s == b"app-extension"
        || s == b"tv-app-extension"
        || s == b"system-extension"
        || s == b"watchkit-extension"
        || s == b"extensionkit-extension"
    {
        (TARGET_KIND_EXTENSION, b"app-extension")
    } else if s == b"library.static"
        || s == b"library.dynamic"
        || s == b"bundle"
        || s == b"bundle.unit-test"
    {
        (TARGET_KIND_LIBRARY, b"library")
    } else {
        (TARGET_KIND_UNKNOWN, b"")
    }
}

fn parse_pbxproj(data: &[u8]) -> Targets {
    let mut targets = Targets::new();
    let begin_marker = b"/* Begin PBXNativeTarget section */";
    let end_marker = b"/* End PBXNativeTarget section */";
    let section_start = match find_from(data, 0, begin_marker) {
        Some(p) => p + begin_marker.len(),
        None => return targets,
    };
    let section_end = find_from(data, section_start, end_marker).unwrap_or(data.len());
    let section = &data[section_start..section_end];
    let mut pos = 0;
    while pos < section.len() {
        let name_eq = match find_from(section, pos, b"name = \"") {
            Some(p) => p,
            None => break,
        };
        let name_start = name_eq + b"name = \"".len();
        let name_end = match find_from(section, name_start, b"\"") {
            Some(p) => p,
            None => break,
        };
        let name = &section[name_start..name_end];
        let stanza_end = find_from(section, name_end, b"};").unwrap_or(section.len());
        let (kind, subtype) = if let Some(pt_pos) = find_from(section, name_end, b"productType = \"") {
            if pt_pos < stanza_end {
                let val_start = pt_pos + b"productType = \"".len();
                let val_end = find_from(section, val_start, b"\"").unwrap_or(stanza_end);
                product_type_info(&section[val_start..val_end])
            } else {
                (TARGET_KIND_UNKNOWN, &b""[..])
            }
        } else {
            (TARGET_KIND_UNKNOWN, &b""[..])
        };
        if !name.is_empty() {
            targets.push(name, kind, subtype);
        }
        pos = stanza_end + 2;
    }
    targets
}

// ── Project kind constants ────────────────────────────────────────────────────

const KIND_SPM: u8 = 0;
const KIND_XCODEPROJ: u8 = 1;
const KIND_XCWORKSPACE: u8 = 2;

// ── CAP_PROJECT_MODEL hook ────────────────────────────────────────────────────

#[basalt_plugin]
fn build_project_model(root: &str) -> Vec<u8> {
    let root_bytes = root.as_bytes();
    let ws_name = root
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(root);

    if root.ends_with(".xcodeproj") {
        let pbxproj_path = join_path(root_bytes, b"project.pbxproj");
        if !pbxproj_path.is_empty() {
            let data = read_file(pbxproj_path);
            if !data.is_empty() {
                let targets = parse_pbxproj(data);
                let display_name = ws_name.trim_end_matches(".xcodeproj");
                let project_root = root
                    .rsplit_once(['/', '\\'])
                    .map(|(parent, _)| parent)
                    .unwrap_or(root);
                return emit_model(
                    KIND_XCODEPROJ,
                    project_root,
                    "swift",
                    display_name,
                    &utf8_lossy(pbxproj_path),
                    &targets,
                )
                .into_bytes();
            }
        }
    }

    if root.ends_with(".xcworkspace") {
        let xcws_path = join_path(root_bytes, b"contents.xcworkspacedata");
        if !xcws_path.is_empty() {
            let data = read_file(xcws_path);
            if !data.is_empty() {
                let display_name = ws_name.trim_end_matches(".xcworkspace");
                let project_root = root
                    .rsplit_once(['/', '\\'])
                    .map(|(parent, _)| parent)
                    .unwrap_or(root);
                return emit_model(
                    KIND_XCWORKSPACE,
                    project_root,
                    "swift",
                    display_name,
                    &utf8_lossy(xcws_path),
                    &Targets::new(),
                )
                .into_bytes();
            }
        }
    }

    // ── Try SPM ──────────────────────────────────────────────────────────────
    {
        let path = join_path(root_bytes, b"Package.swift");
        if !path.is_empty() {
            let data = read_file(path);
            if !data.is_empty() {
                let targets = parse_package_swift(data);
                let display_name = parse_package_name(data).unwrap_or_else(|| ws_name.to_string());
                return emit_model(
                    KIND_SPM,
                    root,
                    "swift",
                    &display_name,
                    &utf8_lossy(path),
                    &targets,
                )
                .into_bytes();
            }
        }
    }

    // ── Try Xcode project ────────────────────────────────────────────────────
    {
        let mut xcodeproj_suffix = [0u8; 256];
        let n = ws_name.len().min(240);
        xcodeproj_suffix[..n].copy_from_slice(ws_name.as_bytes());
        xcodeproj_suffix[n..n + 26].copy_from_slice(b".xcodeproj/project.pbxproj");
        let suffix_len = n + 26;
        let pbxproj_path = join_path(root_bytes, &xcodeproj_suffix[..suffix_len]);
        if !pbxproj_path.is_empty() {
            let data = read_file(pbxproj_path);
            if !data.is_empty() {
                let targets = parse_pbxproj(data);
                let display_name = ws_name.to_string();
                return emit_model(
                    KIND_XCODEPROJ,
                    root,
                    "swift",
                    &display_name,
                    &utf8_lossy(pbxproj_path),
                    &targets,
                )
                .into_bytes();
            }
        }
    }

    // ── Try Xcode workspace ──────────────────────────────────────────────────
    {
        let mut xcws_suffix = [0u8; 256];
        let n = ws_name.len().min(210);
        xcws_suffix[..n].copy_from_slice(ws_name.as_bytes());
        let tail = b".xcworkspace/contents.xcworkspacedata";
        xcws_suffix[n..n + tail.len()].copy_from_slice(tail);
        let suffix_len = n + tail.len();
        let xcws_path = join_path(root_bytes, &xcws_suffix[..suffix_len]);
        if !xcws_path.is_empty() {
            let data = read_file(xcws_path);
            if !data.is_empty() {
                let display_name = ws_name.to_string();
                return emit_model(
                    KIND_XCWORKSPACE,
                    root,
                    "swift",
                    &display_name,
                    &utf8_lossy(xcws_path),
                    &Targets::new(),
                )
                .into_bytes();
            }
        }
    }

    Vec::new()
}

fn emit_model(
    kind: u8,
    root: &str,
    ecosystem: &str,
    display_name: &str,
    manifest_path: &str,
    targets: &Targets,
) -> String {
    let project_kind = project_kind_for(kind, targets);
    let project_subtype = project_subtype_for(kind);
    let project_capabilities = project_capabilities_for(targets);
    let build_root = if kind == KIND_SPM { ".build/" } else { "DerivedData/" };
    let project_id = sanitize_id(display_name);

    let mut targets_json = String::new();
    for i in 0..targets.count {
        if i > 0 {
            targets_json.push(',');
        }
        let target_name = utf8_lossy(targets.name(i));
        let target_subtype = {
            let raw = targets.subtype(i);
            if raw.is_empty() { None } else { Some(utf8_lossy(raw)) }
        };
        let target_kind = match targets.kinds[i] {
            TARGET_KIND_BINARY => "app",
            TARGET_KIND_TEST => "test",
            TARGET_KIND_FRAMEWORK => "framework",
            TARGET_KIND_EXTENSION => "extension",
            TARGET_KIND_LIBRARY | TARGET_KIND_PROC_MACRO | TARGET_KIND_PLUGIN => "library",
            _ => "target",
        };
        let target_capabilities = target_capabilities_for(targets.kinds[i], target_subtype.as_deref());
        targets_json.push_str(&format!(
            "{{\"id\":\"{}\",\"name\":\"{}\",\"kind\":\"{}\",\"subtype\":{},\"capabilities\":[{}],\"language\":\"swift\",\"source_roots\":[\"Sources/\"],\"test_roots\":[\"Tests/\"],\"build_roots\":[\"{}\"]}}",
            escape_json(&sanitize_id(&target_name)),
            escape_json(&target_name),
            target_kind,
            quote_opt(target_subtype.as_deref()),
            quote_list(&target_capabilities),
            build_root,
        ));
    }

    let owners = [
        if kind == KIND_SPM {
            Some(owner_json("Package.swift", "exact", display_name, &project_id, "manifest"))
        } else {
            None
        },
        Some(owner_json("Sources/", "prefix", display_name, &project_id, "source")),
        Some(owner_json(
            "Tests/",
            "prefix",
            &format!("{display_name} tests"),
            &project_id,
            "test",
        )),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(",");

    format!(
        concat!(
            "{{",
            "\"schema_version\":1,",
            "\"ecosystem\":\"{}\",",
            "\"root\":\"{}\",",
            "\"display_name\":\"{}\",",
            "\"primary_manifest_path\":\"{}\",",
            "\"projects\":[{{",
                "\"id\":\"{}\",",
                "\"name\":\"{}\",",
                "\"kind\":\"{}\",",
                "\"subtype\":{},",
                "\"capabilities\":[{}],",
                "\"manifest_path\":\"{}\",",
                "\"source_roots\":[\"Sources/\"],",
                "\"test_roots\":[\"Tests/\"],",
                "\"build_roots\":[\"{}\"],",
                "\"targets\":[{}]",
            "}}],",
            "\"owners\":[{}]",
            "}}"
        ),
        ecosystem,
        escape_json(root),
        escape_json(display_name),
        escape_json(manifest_path),
        escape_json(&project_id),
        escape_json(display_name),
        project_kind,
        quote_opt(project_subtype),
        quote_list(&project_capabilities),
        escape_json(manifest_path),
        build_root,
        targets_json,
        owners,
    )
}

fn project_kind_for(kind: u8, targets: &Targets) -> &'static str {
    if targets.count > 0 {
        let mut has_app = false;
        let mut has_library = false;
        let mut has_non_test = false;
        for idx in 0..targets.count {
            match targets.kinds[idx] {
                TARGET_KIND_BINARY | TARGET_KIND_FRAMEWORK | TARGET_KIND_EXTENSION => {
                    has_app = true;
                    has_non_test = true;
                }
                TARGET_KIND_LIBRARY | TARGET_KIND_PROC_MACRO | TARGET_KIND_PLUGIN => {
                    has_library = true;
                    has_non_test = true;
                }
                TARGET_KIND_TEST => {}
                _ => has_non_test = true,
            }
        }
        if has_app {
            "app"
        } else if has_library {
            "library"
        } else if !has_non_test {
            "test"
        } else {
            "project"
        }
    } else {
        match kind {
            KIND_XCWORKSPACE => "workspace",
            _ => "project",
        }
    }
}

fn project_subtype_for(kind: u8) -> Option<&'static str> {
    match kind {
        KIND_SPM => Some("swift-package"),
        KIND_XCODEPROJ => Some("xcode-project"),
        KIND_XCWORKSPACE => Some("xcode-workspace"),
        _ => None,
    }
}

fn project_capabilities_for(targets: &Targets) -> Vec<String> {
    let mut caps = Vec::new();
    for idx in 0..targets.count {
        let subtype = {
            let raw = targets.subtype(idx);
            if raw.is_empty() { None } else { Some(utf8_lossy(raw)) }
        };
        for cap in target_capabilities_for(targets.kinds[idx], subtype.as_deref()) {
            if !caps.contains(&cap) {
                caps.push(cap);
            }
        }
    }
    caps
}

fn target_capabilities_for(kind: u8, subtype: Option<&str>) -> Vec<String> {
    let mut caps = match kind {
        TARGET_KIND_BINARY => vec!["is-executable".to_string()],
        TARGET_KIND_TEST => vec!["is-test-only".to_string()],
        TARGET_KIND_PROC_MACRO | TARGET_KIND_PLUGIN => {
            vec!["is-library".to_string(), "builds-plugin".to_string()]
        }
        TARGET_KIND_FRAMEWORK => vec!["is-library".to_string()],
        TARGET_KIND_EXTENSION => vec!["is-executable".to_string()],
        TARGET_KIND_LIBRARY => vec!["is-library".to_string()],
        _ => Vec::new(),
    };
    if matches!(subtype, Some("xcode-app") | Some("app-extension") | Some("app-clip")) {
        if !caps.iter().any(|cap| cap == "has-ui") {
            caps.push("has-ui".to_string());
        }
    }
    caps
}

fn quote_list(items: &[String]) -> String {
    items.iter()
        .map(|item| format!("\"{}\"", escape_json(item)))
        .collect::<Vec<_>>()
        .join(",")
}

fn quote_opt(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{}\"", escape_json(value)),
        None => "null".to_string(),
    }
}
