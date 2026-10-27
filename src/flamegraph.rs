//! Draws a [`Profile`] as a flame graph, the visualization Brendan Gregg
//! introduced and the layout his `flamegraph.pl` draws: one box per frame,
//! stacked root to leaf from the bottom up, each as wide as the share of samples
//! whose stack passed through it. Siblings are sorted by name, not by time, so
//! a frame's position means nothing and its width means everything. The x axis
//! is not time passing.
//!
//! The SVG is self-contained and interactive, as `flamegraph.pl`'s is: hovering
//! a frame names it and its sample count, clicking one zooms the graph to it,
//! and Search (or Ctrl-F) highlights the frames whose name matches a regular
//! expression and reports what share of the samples they cover.
//!
//! Colors follow the palette Gregg uses for JIT-compiled runtimes: the program's
//! own functions green, the runtime underneath them yellow, system libraries
//! red. Within a color, the shade comes from a hash of the name, so a function
//! keeps its shade from one graph to the next.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::profile::{Frame, FrameKind, Profile};

const IMAGE_WIDTH: f64 = 1200.0;
const FRAME_HEIGHT: f64 = 16.0;
const FONT_SIZE: f64 = 12.0;
/// The average width of a character, as a share of the font size.
const FONT_WIDTH: f64 = 0.59;
const X_PAD: f64 = 10.0;
/// The room above the frames, for the title and subtitle.
const Y_PAD_TOP: f64 = FONT_SIZE * 4.0;
/// The room below them, for the line that names the hovered frame.
const Y_PAD_BOTTOM: f64 = FONT_SIZE * 2.0 + 10.0;
/// Frames narrower than this many pixels are left out.
const MIN_WIDTH: f64 = 0.1;

/// A frame and every stack that passed through it.
#[derive(Default)]
struct Node {
    samples: u64,
    children: BTreeMap<Frame, Node>,
}

/// One box of the graph.
struct Placed {
    frame: Frame,
    depth: usize,
    /// The samples to its left on its row.
    offset: u64,
    samples: u64,
}

/// Render `profile` as an SVG flame graph headed by `title` and `subtitle`.
pub fn svg(profile: &Profile, title: &str, subtitle: &str) -> String {
    let mut root = Node::default();
    for (stack, &count) in &profile.stacks {
        root.samples += count;
        let mut node = &mut root;
        for frame in stack {
            node = node.children.entry(frame.clone()).or_default();
            node.samples += count;
        }
    }
    let all = Frame {
        name: "all".to_string(),
        kind: FrameKind::Marker,
    };
    let mut boxes = Vec::new();
    lay_out(&all, &root, 0, 0, &mut boxes);

    let total = root.samples.max(1);
    let per_sample = (IMAGE_WIDTH - 2.0 * X_PAD) / total as f64;
    let depth = boxes.iter().map(|b| b.depth).max().unwrap_or(0);
    let height = (depth + 1) as f64 * FRAME_HEIGHT + Y_PAD_TOP + Y_PAD_BOTTOM;

    let mut out = String::new();
    let _ = write!(
        out,
        r##"<?xml version="1.0" standalone="no"?>
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg version="1.1" width="{IMAGE_WIDTH}" height="{height}" onload="init(evt)" viewBox="0 0 {IMAGE_WIDTH} {height}" xmlns="http://www.w3.org/2000/svg">
<defs>
<linearGradient id="background" y1="0" y2="1" x1="0" x2="0"><stop stop-color="#eeeeee" offset="5%"/><stop stop-color="#eeeeb0" offset="95%"/></linearGradient>
</defs>
<style type="text/css">
text {{ font-family: Verdana, sans-serif; font-size: {FONT_SIZE}px; fill: rgb(0,0,0); }}
#title {{ text-anchor: middle; font-size: 17px; }}
#subtitle {{ text-anchor: middle; fill: rgb(160,160,160); }}
#search, #matched {{ text-anchor: end; }}
#search, #unzoom {{ cursor: pointer; }}
#search:hover, #unzoom:hover, #search.show {{ fill: rgb(230,0,230); }}
#frames > g:hover {{ stroke: black; stroke-width: 0.5; cursor: pointer; }}
.hide {{ display: none; }}
.parent {{ opacity: 0.5; }}
</style>
<script type="text/ecmascript"><![CDATA[{SCRIPT}]]></script>
<rect x="0" y="0" width="{IMAGE_WIDTH}" height="{height}" fill="url(#background)"/>
<text id="title" x="{mid}" y="{title_y}">{title}</text>
<text id="subtitle" x="{mid}" y="{subtitle_y}">{subtitle}</text>
<text id="unzoom" class="hide" x="{X_PAD}" y="{title_y}">Reset Zoom</text>
<text id="search" x="{search_x}" y="{title_y}">Search</text>
<text id="matched" x="{search_x}" y="{details_y}"> </text>
<text id="details" x="{X_PAD}" y="{details_y}"> </text>
<g id="frames" data-total="{total}">
"##,
        mid = IMAGE_WIDTH / 2.0,
        title_y = FONT_SIZE * 2.0,
        subtitle_y = FONT_SIZE * 3.0 + 2.0,
        search_x = IMAGE_WIDTH - X_PAD,
        details_y = height - FONT_SIZE / 2.0 - 4.0,
        title = escape(title),
        subtitle = escape(subtitle),
        SCRIPT = SCRIPT,
    );
    for b in &boxes {
        let width = b.samples as f64 * per_sample;
        if width < MIN_WIDTH {
            continue;
        }
        let x = X_PAD + b.offset as f64 * per_sample;
        let y = height - Y_PAD_BOTTOM - (b.depth + 1) as f64 * FRAME_HEIGHT;
        let fill = color(&b.frame);
        let samples = if b.samples == 1 { "sample" } else { "samples" };
        let percent = 100.0 * b.samples as f64 / total as f64;
        let _ = writeln!(
            out,
            r#"<g data-x="{offset}" data-w="{count}"><title>{name} ({count} {samples}, {percent:.2}%)</title><rect x="{x:.2}" y="{y}" width="{width:.2}" height="{h}" fill="{fill}" data-fill="{fill}" rx="2" ry="2"/><text x="{tx:.2}" y="{ty}">{label}</text></g>"#,
            offset = b.offset,
            count = b.samples,
            name = escape(&b.frame.name),
            h = FRAME_HEIGHT - 1.0,
            tx = x + 3.0,
            ty = y + FRAME_HEIGHT / 2.0 + 3.5,
            label = escape(&label(&b.frame.name, width)),
        );
    }
    out.push_str("</g>\n</svg>\n");
    out
}

/// Push a box for `node` and, depth first, for everything above it.
fn lay_out(frame: &Frame, node: &Node, depth: usize, offset: u64, boxes: &mut Vec<Placed>) {
    boxes.push(Placed {
        frame: frame.clone(),
        depth,
        offset,
        samples: node.samples,
    });
    let mut child_offset = offset;
    for (child_frame, child) in &node.children {
        lay_out(child_frame, child, depth + 1, child_offset, boxes);
        child_offset += child.samples;
    }
}

/// `name` cut to what fits in a box `width` pixels wide, ending `..` when cut,
/// and empty when not even a few characters fit. The script redoes this with
/// the real text metrics whenever a zoom changes the width.
fn label(name: &str, width: f64) -> String {
    let fits = (width / (FONT_SIZE * FONT_WIDTH)) as usize;
    if fits < 3 {
        String::new()
    } else if name.chars().count() <= fits {
        name.to_string()
    } else {
        let kept: String = name.chars().take(fits - 2).collect();
        format!("{kept}..")
    }
}

/// The fill for `frame`: its kind's hue, shaded by a hash of its name.
fn color(frame: &Frame) -> String {
    let v1 = name_hash(frame.name.chars());
    let v2 = name_hash(frame.name.chars().rev());
    let shade = |base: f64, spread: f64, v: f64| (base + spread * v) as u8;
    let (r, g, b) = match frame.kind {
        FrameKind::Aipl => (
            shade(50.0, 60.0, v1),
            shade(200.0, 55.0, v1),
            shade(50.0, 60.0, v1),
        ),
        FrameKind::Runtime => (
            shade(175.0, 55.0, v1),
            shade(175.0, 55.0, v1),
            shade(50.0, 20.0, v1),
        ),
        FrameKind::System => (
            shade(200.0, 55.0, v1),
            shade(50.0, 80.0, v2),
            shade(50.0, 80.0, v2),
        ),
        FrameKind::Marker => (
            shade(190.0, 20.0, v1),
            shade(190.0, 20.0, v1),
            shade(190.0, 20.0, v1),
        ),
    };
    format!("rgb({r},{g},{b})")
}

/// `flamegraph.pl`'s name hash: a value in [0, 1] weighted toward the first few
/// characters, so names sharing a prefix get similar shades.
fn name_hash(chars: impl Iterator<Item = char>) -> f64 {
    let (mut vector, mut weight, mut max, mut modulus) = (0.0, 1.0, 1.0, 10u32);
    for c in chars {
        let i = (c as u32 % modulus) as f64;
        vector += i / (modulus - 1) as f64 * weight;
        max += weight;
        weight *= 0.70;
        modulus += 1;
        if modulus > 12 {
            break;
        }
    }
    1.0 - vector / max
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Zoom, search, and the hovered frame's details. A box keeps its place in
/// samples in `data-x`/`data-w`; zooming lays boxes out again from those.
const SCRIPT: &str = r#"
var frames, details, searchButton, unzoomButton, matched, total, searching = null;
var X_PAD = 10, WIDTH = 1200, FONT_SIZE = 12, FONT_WIDTH = 0.59;
function init(evt) {
    frames = document.getElementById("frames");
    details = document.getElementById("details").firstChild;
    searchButton = document.getElementById("search");
    unzoomButton = document.getElementById("unzoom");
    matched = document.getElementById("matched");
    total = +frames.getAttribute("data-total");
    for (var g of frames.children) update_text(g);
}
function frame_of(node) {
    while (node && node.parentNode !== frames) node = node.parentNode;
    return node;
}
function name_of(g) {
    var title = g.querySelector("title").textContent;
    return title.substring(0, title.lastIndexOf(" ("));
}
window.addEventListener("click", function(e) {
    var g = frame_of(e.target);
    if (g) {
        if (g.classList.contains("parent")) unzoom();
        zoom(g);
    } else if (e.target.id === "unzoom") {
        unzoom();
    } else if (e.target.id === "search") {
        search_prompt();
    }
});
window.addEventListener("mouseover", function(e) {
    var g = frame_of(e.target);
    if (g) details.nodeValue = "Function: " + g.querySelector("title").textContent;
});
window.addEventListener("mouseout", function(e) {
    if (frame_of(e.target)) details.nodeValue = " ";
});
window.addEventListener("keydown", function(e) {
    if (e.keyCode === 114 || ((e.ctrlKey || e.metaKey) && e.keyCode === 70)) {
        e.preventDefault();
        search_prompt();
    }
});
function update_text(g) {
    var rect = g.querySelector("rect"), text = g.querySelector("text");
    var room = +rect.getAttribute("width") - 3, name = name_of(g);
    text.setAttribute("x", +rect.getAttribute("x") + 3);
    if (room < 3 * FONT_SIZE * FONT_WIDTH) { text.textContent = ""; return; }
    text.textContent = name;
    if (text.getComputedTextLength() <= room) return;
    for (var n = name.length - 1; n > 0; n--) {
        text.textContent = name.substring(0, n) + "..";
        if (text.getComputedTextLength() <= room) return;
    }
    text.textContent = "";
}
function place(g, x, width) {
    var rect = g.querySelector("rect");
    rect.setAttribute("x", x);
    rect.setAttribute("width", width);
    update_text(g);
}
function zoom(target) {
    var x = +target.getAttribute("data-x"), w = +target.getAttribute("data-w");
    var y = +target.querySelector("rect").getAttribute("y");
    var scale = (WIDTH - 2 * X_PAD) / w;
    unzoomButton.classList.remove("hide");
    for (var g of frames.children) {
        var gx = +g.getAttribute("data-x"), gw = +g.getAttribute("data-w");
        var gy = +g.querySelector("rect").getAttribute("y");
        g.classList.remove("hide", "parent");
        if (gy > y) {
            if (gx <= x && gx + gw >= x + w) {
                g.classList.add("parent");
                place(g, X_PAD, WIDTH - 2 * X_PAD);
            } else {
                g.classList.add("hide");
            }
        } else if (gx < x || gx >= x + w) {
            g.classList.add("hide");
        } else {
            place(g, X_PAD + (gx - x) * scale, gw * scale);
        }
    }
}
function unzoom() {
    var scale = (WIDTH - 2 * X_PAD) / total;
    unzoomButton.classList.add("hide");
    for (var g of frames.children) {
        g.classList.remove("hide", "parent");
        place(g, X_PAD + +g.getAttribute("data-x") * scale, +g.getAttribute("data-w") * scale);
    }
}
function search_prompt() {
    if (searching) {
        searching = null;
        for (var g of frames.children) {
            var rect = g.querySelector("rect");
            rect.setAttribute("fill", rect.getAttribute("data-fill"));
        }
        searchButton.classList.remove("show");
        searchButton.firstChild.nodeValue = "Search";
        matched.firstChild.nodeValue = " ";
        return;
    }
    var term = prompt("Enter a search term (regexp allowed, eg: ^str_)", "");
    if (term) search(term);
}
function search(term) {
    var re = new RegExp(term), hits = [];
    for (var g of frames.children) {
        if (!re.test(name_of(g))) continue;
        g.querySelector("rect").setAttribute("fill", "rgb(230,0,230)");
        hits.push([+g.getAttribute("data-x"), +g.getAttribute("data-w")]);
    }
    if (!hits.length) return;
    searching = term;
    searchButton.classList.add("show");
    searchButton.firstChild.nodeValue = "Reset Search";
    // A match inside another match is already counted by it.
    hits.sort(function(a, b) { return a[0] - b[0] || b[1] - a[1]; });
    var covered = 0, end = -1;
    for (var hit of hits) {
        if (hit[0] >= end) { covered += hit[1]; end = hit[0] + hit[1]; }
    }
    matched.firstChild.nodeValue = "Matched: " + (100 * covered / total).toFixed(1) + "%";
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(name: &str, kind: FrameKind) -> Frame {
        Frame {
            name: name.to_string(),
            kind,
        }
    }

    /// Each box's tooltip, `x` and width, in drawing order.
    fn boxes(svg: &str) -> Vec<String> {
        svg.lines()
            .filter(|l| l.starts_with("<g data-x="))
            .map(|l| {
                let title = &l[l.find("<title>").unwrap() + 7..l.find("</title>").unwrap()];
                let attr = |name: &str| {
                    let at = l.find(&format!(" {name}=\"")).unwrap() + name.len() + 3;
                    l[at..at + l[at..].find('"').unwrap()].to_string()
                };
                format!("{title} x={} width={}", attr("x"), attr("width"))
            })
            .collect()
    }

    #[test]
    fn stacks_merge_by_prefix_and_siblings_sort_by_name() {
        let main = frame("main", FrameKind::Aipl);
        let profile = Profile {
            stacks: BTreeMap::from([
                (vec![main.clone(), frame("work", FrameKind::Aipl)], 3),
                (
                    vec![
                        main.clone(),
                        frame("build", FrameKind::Aipl),
                        frame("malloc", FrameKind::System),
                    ],
                    1,
                ),
                (vec![main.clone()], 1),
            ]),
            hz: 997,
            lost: 0,
        };
        assert_eq!(
            boxes(&svg(&profile, "Flame Graph", "")),
            [
                "all (5 samples, 100.00%) x=10.00 width=1180.00",
                "main (5 samples, 100.00%) x=10.00 width=1180.00",
                "build (1 sample, 20.00%) x=10.00 width=236.00",
                "malloc (1 sample, 20.00%) x=10.00 width=236.00",
                "work (3 samples, 60.00%) x=246.00 width=708.00",
            ]
        );
    }

    #[test]
    fn a_name_is_cut_to_its_box_and_escaped() {
        assert_eq!(label("Vec<str>::push", 1000.0), "Vec<str>::push");
        assert_eq!(label("Vec<str>::push", 50.0), "Vec<s..");
        assert_eq!(label("push", 20.0), "");
        assert_eq!(escape("Vec<&str>"), "Vec&lt;&amp;str&gt;");
    }
}
