//! Tier-0 project-model plugin for Swift/Xcode workspaces.
//!
//! Detects Swift projects (SPM, Xcode project, Xcode workspace) and publishes
//! a `SwiftProjectModel` under the capability ID `"project-model:swift"`.
//!
//! ## Wire format (`SwiftProjectModel`)
//!
//! ```text
//! [kind: u8]                         project kind: 0=spm, 1=xcodeproj, 2=xcworkspace
//! [name_len: u16][name: bytes]        project/workspace name
//! [target_count: u16]
//!   for each target:
//!     [kind: u8]                      target kind
//!     [target_name_len: u16][target_name: bytes]
//! ```
//!
//! # Build & install
//!
//! ```sh
//! cargo build --target wasm32-unknown-unknown --release
//! cp target/wasm32-unknown-unknown/release/swift_project_model.wasm \
//!    ~/.config/basalt/plugins/swift-project-model.wasm
//! ```

use basalt_plugin_sdk::prelude::*;

// ── Plugin identity & metadata ────────────────────────────────────────────────

basalt_plugin_meta! {
    name:         "swift-project-model",
    version:      "0.1.0",
    hook_flags:   CAP_PROJECT_MODEL,
    provides:     "project-model:swift",
    requires:     "",
    file_globs:   "",
    activates_on: "**/Package.swift\n**/*.xcodeproj\n**/*.xcworkspace",
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

// ── Wire-format helpers (Vec<u8>-based) ───────────────────────────────────────

fn write_u8_v(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

fn write_u16_v(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_str_v(out: &mut Vec<u8>, s: &[u8]) {
    let len = s.len().min(0xFFFF);
    write_u16_v(out, len as u16);
    out.extend_from_slice(&s[..len]);
}

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

// ── Target extraction ─────────────────────────────────────────────────────────

const MAX_TARGETS: usize = 64;

struct Targets {
    names: [[u8; 128]; MAX_TARGETS],
    lens: [usize; MAX_TARGETS],
    kinds: [u8; MAX_TARGETS],
    count: usize,
}

impl Targets {
    fn new() -> Self {
        Self {
            names: [[0u8; 128]; MAX_TARGETS],
            lens: [0; MAX_TARGETS],
            kinds: [TARGET_KIND_UNKNOWN; MAX_TARGETS],
            count: 0,
        }
    }

    fn push(&mut self, name: &[u8], kind: u8) {
        if self.count >= MAX_TARGETS {
            return;
        }
        let len = name.len().min(128);
        self.names[self.count][..len].copy_from_slice(&name[..len]);
        self.lens[self.count] = len;
        self.kinds[self.count] = kind;
        self.count += 1;
    }

    fn name(&self, i: usize) -> &[u8] {
        &self.names[i][..self.lens[i]]
    }
}

// ── SPM parsing ───────────────────────────────────────────────────────────────

const SPM_KEYWORDS: &[(&[u8], u8)] = &[
    (b".executableTarget(", TARGET_KIND_BINARY),
    (b".testTarget(", TARGET_KIND_TEST),
    (b".macro(", TARGET_KIND_PROC_MACRO),
    (b".plugin(", TARGET_KIND_PLUGIN),
    (b".binaryTarget(", TARGET_KIND_FRAMEWORK),
    (b".target(", TARGET_KIND_LIBRARY),
];

fn parse_package_swift(data: &[u8]) -> Targets {
    let mut targets = Targets::new();
    let mut pos = 0;
    while pos < data.len() {
        let mut found: Option<(usize, u8)> = None;
        for &(kw, kind) in SPM_KEYWORDS {
            if let Some(p) = find_from(data, pos, kw) {
                match found {
                    None => found = Some((p, kind)),
                    Some((q, _)) if p < q => found = Some((p, kind)),
                    _ => {}
                }
            }
        }
        let (kw_pos, kind) = match found {
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
            targets.push(name, kind);
        }
        pos = kw_pos + 1;
    }
    targets
}

// ── Xcode parsing ─────────────────────────────────────────────────────────────

fn product_type_kind(product_type: &[u8]) -> u8 {
    const PREFIX: &[u8] = b"com.apple.product-type.";
    let s = if product_type.len() > PREFIX.len() && &product_type[..PREFIX.len()] == PREFIX {
        &product_type[PREFIX.len()..]
    } else {
        product_type
    };
    if s == b"application" || s == b"tool" || s == b"commandline-tool" || s == b"app-clip" {
        TARGET_KIND_BINARY
    } else if s == b"framework" || s == b"static-framework" {
        TARGET_KIND_FRAMEWORK
    } else if s == b"unit-test-bundle" || s == b"ui-testing-bundle" {
        TARGET_KIND_TEST
    } else if s == b"app-extension"
        || s == b"tv-app-extension"
        || s == b"system-extension"
        || s == b"watchkit-extension"
        || s == b"extensionkit-extension"
    {
        TARGET_KIND_EXTENSION
    } else if s == b"library.static"
        || s == b"library.dynamic"
        || s == b"bundle"
        || s == b"bundle.unit-test"
    {
        TARGET_KIND_LIBRARY
    } else {
        TARGET_KIND_UNKNOWN
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
        let kind = if let Some(pt_pos) = find_from(section, name_end, b"productType = \"") {
            if pt_pos < stanza_end {
                let val_start = pt_pos + b"productType = \"".len();
                let val_end = find_from(section, val_start, b"\"").unwrap_or(stanza_end);
                product_type_kind(&section[val_start..val_end])
            } else {
                TARGET_KIND_UNKNOWN
            }
        } else {
            TARGET_KIND_UNKNOWN
        };
        if !name.is_empty() {
            targets.push(name, kind);
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
    let root = root.as_bytes();
    let name_start = root
        .iter()
        .rposition(|&b| b == b'/' || b == b'\\')
        .map(|i| i + 1)
        .unwrap_or(0);
    let ws_name = &root[name_start..];

    // ── Try SPM ──────────────────────────────────────────────────────────────
    {
        let path = join_path(root, b"Package.swift");
        if !path.is_empty() {
            let data = read_file(path);
            if !data.is_empty() {
                let targets = parse_package_swift(data);
                return emit_model(KIND_SPM, ws_name, &targets);
            }
        }
    }

    // ── Try Xcode project ────────────────────────────────────────────────────
    {
        let mut xcodeproj_suffix = [0u8; 256];
        let n = ws_name.len().min(240);
        xcodeproj_suffix[..n].copy_from_slice(&ws_name[..n]);
        xcodeproj_suffix[n..n + 26].copy_from_slice(b".xcodeproj/project.pbxproj");
        let suffix_len = n + 26;
        let pbxproj_path = join_path(root, &xcodeproj_suffix[..suffix_len]);
        if !pbxproj_path.is_empty() {
            let data = read_file(pbxproj_path);
            if !data.is_empty() {
                let targets = parse_pbxproj(data);
                return emit_model(KIND_XCODEPROJ, ws_name, &targets);
            }
        }
    }

    // ── Try Xcode workspace ──────────────────────────────────────────────────
    {
        let mut xcws_suffix = [0u8; 256];
        let n = ws_name.len().min(210);
        xcws_suffix[..n].copy_from_slice(&ws_name[..n]);
        let tail = b".xcworkspace/contents.xcworkspacedata";
        xcws_suffix[n..n + tail.len()].copy_from_slice(tail);
        let suffix_len = n + tail.len();
        let xcws_path = join_path(root, &xcws_suffix[..suffix_len]);
        if !xcws_path.is_empty() {
            let data = read_file(xcws_path);
            if !data.is_empty() {
                return emit_model(KIND_XCWORKSPACE, ws_name, &Targets::new());
            }
        }
    }

    Vec::new()
}

fn emit_model(kind: u8, name: &[u8], targets: &Targets) -> Vec<u8> {
    let mut out = Vec::new();
    write_u8_v(&mut out, kind);
    write_str_v(&mut out, name);
    write_u16_v(&mut out, targets.count as u16);
    for i in 0..targets.count {
        write_u8_v(&mut out, targets.kinds[i]);
        write_str_v(&mut out, targets.name(i));
    }
    out
}
