//! The ADP parser (`nsd/adpparse.c`).
//!
//! Splits a page into text and script blocks:
//!
//! * `<% script %>` — evaluated, result discarded (nesting of `<% %>` inside
//!   is honoured when finding the end);
//! * `<%= script %>` — evaluated and the result appended
//!   (`ns_adp_append script`);
//! * `<script runat=server [language=tcl] [stream=on]> ... </script>`;
//! * registered tags (`ns_adp_registertag` & co.), converted into a script
//!   block calling the tag's handler.

use std::collections::HashMap;

use crate::server::{AdpTag, AdpTagKind};

/// One block of a parsed page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Literal text.
    Text(String),
    /// A script to evaluate.
    Script(String),
}

/// A parsed page (`AdpCode`): blocks and the line each starts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdpCode {
    /// Blocks in order.
    pub blocks: Vec<(Block, usize)>,
}

impl AdpCode {
    /// Number of script blocks.
    pub fn nscripts(&self) -> usize {
        self.blocks
            .iter()
            .filter(|(b, _)| matches!(b, Block::Script(_)))
            .count()
    }
}

/// Options affecting parsing.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOpts {
    /// Drop all scripts (`ADP_SAFE`).
    pub safe: bool,
    /// Combine everything into one script (`ADP_SINGLE`).
    pub single: bool,
}

struct Parser {
    code: AdpCode,
    line: usize,
}

impl Parser {
    fn append(&mut self, s: &str, script: bool) {
        if s.is_empty() {
            return;
        }
        let block = if script {
            Block::Script(s.to_owned())
        } else {
            Block::Text(s.to_owned())
        };
        self.code.blocks.push((block, self.line));
        self.line += s.bytes().filter(|&b| b == b'\n').count();
    }
}

/// A Tcl list element quoting for generated scripts (brace-quoting with
/// backslash fallback), sufficient for `Tcl_DStringAppendElement` output.
pub fn tcl_quote(s: &str) -> String {
    if s.is_empty() {
        return "{}".to_owned();
    }
    let needs = s.chars().any(|c| {
        matches!(
            c,
            ' ' | '\t' | '\n' | '\r' | '{' | '}' | '[' | ']' | '$' | '"' | ';' | '\\'
        )
    }) || s.starts_with('#');
    if !needs {
        return s.to_owned();
    }
    let mut depth: i32 = 0;
    let mut balanced = true;
    let mut prev_backslash = false;
    for c in s.chars() {
        if prev_backslash {
            prev_backslash = false;
            continue;
        }
        match c {
            '\\' => prev_backslash = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    balanced = false;
                }
            }
            _ => {}
        }
    }
    if balanced && depth == 0 && !s.ends_with('\\') {
        return format!("{{{s}}}");
    }
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            ' ' | '{' | '}' | '[' | ']' | '$' | '"' | ';' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Lower-cased tag name and the attribute start offset for a tag spanning
/// `s[start..end]` where `s[start] == '<'` and `s[end] == '>'` (`GetTag`).
fn get_tag(s: &str, start: usize, end: usize) -> (String, usize) {
    let inner = &s[start + 1..end];
    let trimmed = inner.trim_start();
    let off = start + 1 + (inner.len() - trimmed.len());
    let name_len = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let name = trimmed[..name_len].to_lowercase();
    let rest = &trimmed[name_len..];
    let attr = off + name_len + (rest.len() - rest.trim_start().len());
    (name, attr)
}

/// Parse attributes into `(name, value)` pairs (`ParseAtts`).  A bare
/// attribute has its name as value.
pub fn parse_atts(a: &str) -> Vec<(String, String)> {
    let b = a.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let unquote = |v: &str| -> String {
        let vb = v.as_bytes();
        if vb.len() >= 2 && (vb[0] == b'"' || vb[0] == b'\'') && vb[vb.len() - 1] == vb[0] {
            v[1..v.len() - 1].to_owned()
        } else {
            v.to_owned()
        }
    };
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let as_ = i;
        if b[i] == b'"' || b[i] == b'\'' {
            let q = b[i];
            i += 1;
            while i < b.len() && b[i] != q {
                i += 1;
            }
            i = (i + 1).min(b.len());
        } else {
            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' {
                i += 1;
            }
        }
        let name = unquote(&a[as_..i]);
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let vs = i;
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                i = (i + 1).min(b.len());
            } else {
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
            }
            out.push((name, unquote(&a[vs..i])));
        } else {
            out.push((name.clone(), name));
        }
    }
    out
}

/// Whether `<script ...>` attributes select server-side Tcl, and whether
/// streaming was requested (`GetScript`).
fn script_attrs(atts: &str) -> Option<bool> {
    let mut runat = false;
    let mut nottcl = false;
    let mut stream = false;
    for (k, v) in parse_atts(atts) {
        if k.eq_ignore_ascii_case("runat") && v.eq_ignore_ascii_case("server") {
            runat = true;
        } else if k.eq_ignore_ascii_case("language") && !v.eq_ignore_ascii_case("tcl") {
            nottcl = true;
        } else if k.eq_ignore_ascii_case("stream") && v.eq_ignore_ascii_case("on") {
            stream = true;
        }
    }
    (runat && !nottcl).then_some(stream)
}

fn tag_script(tag: &AdpTag, atts: &str, content: Option<&str>) -> String {
    let mut s = String::from("ns_adp_append [");
    match &tag.kind {
        AdpTagKind::Proc(p) => {
            s.push_str(&tcl_quote(p));
            for (_, v) in parse_atts(atts) {
                s.push(' ');
                s.push_str(&tcl_quote(&v));
            }
            if let Some(c) = content {
                s.push(' ');
                s.push_str(&tcl_quote(c));
            }
        }
        AdpTagKind::Adp(text) | AdpTagKind::Script(text) => {
            if matches!(tag.kind, AdpTagKind::Adp(_)) {
                s.push_str("ns_adp_eval ");
            }
            s.push_str(&tcl_quote(text));
            if let Some(c) = content {
                s.push(' ');
                s.push_str(&tcl_quote(c));
            }
            s.push_str(" [ns_set create ");
            s.push_str(&tcl_quote(&tag.name));
            for (k, v) in parse_atts(atts) {
                s.push(' ');
                s.push_str(&tcl_quote(&k));
                s.push(' ');
                s.push_str(&tcl_quote(&v));
            }
            s.push(']');
        }
    }
    s.push(']');
    s
}

enum State<'t> {
    Next,
    Script {
        body_start: usize,
        level: usize,
        stream: bool,
    },
    Reg {
        tag: &'t AdpTag,
        atts: String,
        content_start: usize,
        level: usize,
    },
}

/// Parse an ADP page (`NsAdpParse`).
pub fn parse(adp: &str, tags: &HashMap<String, AdpTag>, opts: ParseOpts) -> AdpCode {
    let mut p = Parser {
        code: AdpCode::default(),
        line: 0,
    };
    let mut text = 0usize;
    let mut pos = 0usize;
    let mut state = State::Next;
    let mut stream_done = false;
    while let Some(lt) = adp[pos..].find('<').map(|i| pos + i) {
        let Some(gt) = adp[lt..].find('>').map(|i| lt + i) else {
            break;
        };
        let mut next = lt + 1;
        match state {
            State::Next => {
                let b = adp.as_bytes();
                if b.get(lt + 1) == Some(&b'%') && b.get(lt + 2) != Some(&b'>') {
                    // Find the matching %>, honouring nested <% %>.
                    let search_from = gt.saturating_sub(1).max(lt + 2);
                    let mut end = adp[search_from..].find("%>").map(|i| search_from + i);
                    let mut level = 0;
                    let mut n = lt + 2;
                    while let (Some(e), Some(o)) = (end, adp[n..].find("<%").map(|i| n + i)) {
                        if o >= e {
                            break;
                        }
                        level += 1;
                        n = o + 2;
                    }
                    while let (Some(e), true) = (end, level > 0) {
                        end = adp[e + 2..].find("%>").map(|i| e + 2 + i);
                        level -= 1;
                    }
                    if let Some(e) = end {
                        p.append(&adp[text..lt], false);
                        if !opts.safe {
                            if b.get(lt + 2) == Some(&b'=') {
                                p.append(&format!("ns_adp_append {}", &adp[lt + 3..e]), true);
                            } else {
                                p.append(&adp[lt + 2..e], true);
                            }
                        }
                        text = e + 2;
                    } else {
                        p.append(&adp[text..lt + 2], false);
                        text = lt + 2;
                    }
                    next = text;
                } else {
                    let (name, attr) = get_tag(adp, lt, gt);
                    let atts = &adp[attr.min(gt)..gt];
                    if name == "script" {
                        if let Some(stream) = script_attrs(atts) {
                            p.append(&adp[text..lt], false);
                            state = State::Script {
                                body_start: gt + 1,
                                level: 1,
                                stream,
                            };
                            pos = gt + 1;
                            continue;
                        }
                    }
                    if let Some(tag) = tags.get(&name) {
                        p.append(&adp[text..lt], false);
                        if tag.endtag.is_none() {
                            p.append(&tag_script(tag, atts, None), true);
                            text = gt + 1;
                            next = text;
                        } else {
                            state = State::Reg {
                                tag,
                                atts: atts.to_owned(),
                                content_start: gt + 1,
                                level: 1,
                            };
                            pos = gt + 1;
                            continue;
                        }
                    }
                }
            }
            State::Script {
                body_start,
                ref mut level,
                stream,
            } => {
                let (name, _) = get_tag(adp, lt, gt);
                if name == "script" {
                    *level += 1;
                } else if name == "/script" {
                    *level -= 1;
                    if *level == 0 {
                        if !opts.safe {
                            if stream && !stream_done {
                                p.append("ns_adp_stream", true);
                                stream_done = true;
                            }
                            p.append(&adp[body_start..lt], true);
                        }
                        text = gt + 1;
                        next = text;
                        state = State::Next;
                    }
                }
            }
            State::Reg {
                tag,
                ref atts,
                content_start,
                ref mut level,
            } => {
                let (name, _) = get_tag(adp, lt, gt);
                if name == tag.name {
                    *level += 1;
                } else if Some(&name) == tag.endtag.as_ref() {
                    *level -= 1;
                    if *level == 0 {
                        let script = tag_script(tag, atts, Some(&adp[content_start..lt]));
                        p.append(&script, true);
                        text = gt + 1;
                        next = text;
                        state = State::Next;
                    }
                }
            }
        }
        pos = next;
    }
    p.append(&adp[text..], false);
    if opts.single {
        let mut script = String::new();
        for (b, _) in &p.code.blocks {
            match b {
                Block::Script(s) => script.push_str(s),
                Block::Text(t) => {
                    script.push_str("ns_adp_append ");
                    script.push_str(&tcl_quote(t));
                }
            }
            script.push('\n');
        }
        p.code.blocks = vec![(Block::Script(script), 0)];
    }
    p.code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(adp: &str) -> Vec<Block> {
        parse(adp, &HashMap::new(), ParseOpts::default())
            .blocks
            .into_iter()
            .map(|(b, _)| b)
            .collect()
    }

    fn t(s: &str) -> Block {
        Block::Text(s.into())
    }

    fn s(s: &str) -> Block {
        Block::Script(s.into())
    }

    #[test]
    fn scripts_and_text() {
        assert_eq!(
            blocks("a<% set x 1 %>b<%= $x %>c"),
            vec![
                t("a"),
                s(" set x 1 "),
                t("b"),
                s("ns_adp_append  $x "),
                t("c")
            ]
        );
        assert_eq!(blocks("plain"), vec![t("plain")]);
        assert_eq!(blocks("<b>bold</b>"), vec![t("<b>bold</b>")]);
        assert_eq!(
            blocks("unterminated <% foo"),
            vec![t("unterminated <% foo")]
        );
        assert_eq!(
            blocks("open <% foo > bar"),
            vec![t("open <%"), t(" foo > bar")]
        );
        assert_eq!(blocks("<%>"), vec![t("<%>")]);
    }

    #[test]
    fn nested_percent_tags() {
        assert_eq!(
            blocks("<% if 1 { %>x<% } %>"),
            vec![s(" if 1 { "), t("x"), s(" } ")]
        );
        assert_eq!(
            blocks("<% a <% b %> c %>z"),
            vec![s(" a <% b %> c "), t("z")]
        );
    }

    #[test]
    fn script_tags() {
        assert_eq!(
            blocks("x<script runat=server>ns_adp_puts hi</script>y"),
            vec![t("x"), s("ns_adp_puts hi"), t("y")]
        );
        assert_eq!(
            blocks("<script>client</script>"),
            vec![t("<script>client</script>")]
        );
        assert_eq!(
            blocks("<script runat=\"server\" language=javascript>js</script>"),
            vec![t(
                "<script runat=\"server\" language=javascript>js</script>"
            )]
        );
        assert_eq!(
            blocks("<SCRIPT RUNAT=Server stream=on>a</SCRIPT>"),
            vec![s("ns_adp_stream"), s("a")]
        );
    }

    #[test]
    fn registered_tags() {
        let mut tags = HashMap::new();
        tags.insert(
            "hello".to_owned(),
            AdpTag {
                name: "hello".into(),
                endtag: None,
                kind: AdpTagKind::Proc("hello_proc".into()),
            },
        );
        tags.insert(
            "box".to_owned(),
            AdpTag {
                name: "box".into(),
                endtag: Some("/box".into()),
                kind: AdpTagKind::Script("box_script".into()),
            },
        );
        let code = parse(
            "<hello name=\"a b\">-<box color=red>in<box>x</box></box>",
            &tags,
            ParseOpts::default(),
        );
        let b: Vec<Block> = code.blocks.into_iter().map(|(b, _)| b).collect();
        assert_eq!(b[0], s("ns_adp_append [hello_proc {a b}]"));
        assert_eq!(b[1], t("-"));
        assert_eq!(
            b[2],
            s("ns_adp_append [box_script in<box>x</box> [ns_set create box color red]]")
        );
    }

    #[test]
    fn safe_and_single() {
        let tags = HashMap::new();
        let safe = parse(
            "a<% evil %>b",
            &tags,
            ParseOpts {
                safe: true,
                single: false,
            },
        );
        assert_eq!(safe.nscripts(), 0);
        let single = parse(
            "a<% set x 1 %>b",
            &tags,
            ParseOpts {
                safe: false,
                single: true,
            },
        );
        assert_eq!(single.blocks.len(), 1);
        assert_eq!(
            single.blocks[0].0,
            s("ns_adp_append a\n set x 1 \nns_adp_append b\n")
        );
    }

    #[test]
    fn line_numbers() {
        let code = parse(
            "l0\nl1\n<% x %>\n<% y %>",
            &HashMap::new(),
            ParseOpts::default(),
        );
        let lines: Vec<usize> = code.blocks.iter().map(|(_, l)| *l).collect();
        assert_eq!(lines, vec![0, 2, 2, 3]);
    }

    #[test]
    fn attributes() {
        assert_eq!(
            parse_atts(" a=1 b = 'x y' c \"d\"=\"e\""),
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "x y".into()),
                ("c".into(), "c".into()),
                ("d".into(), "e".into())
            ]
        );
        assert_eq!(tcl_quote(""), "{}");
        assert_eq!(tcl_quote("ab"), "ab");
        assert_eq!(tcl_quote("a b"), "{a b}");
        assert_eq!(tcl_quote("a}b"), "a\\}b");
        assert_eq!(tcl_quote("#x"), "{#x}");
    }
}
