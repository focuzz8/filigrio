//! `MockExtractor` — the original line-heuristic stub. Retained as the fallback
//! for languages without a real extractor yet (no AST). Handles any code
//! artifact so the pipeline never stalls on an unknown language.

use filigrio_core::relation::{CALLS, CONTAINS};
use filigrio_core::{
    Artifact, ArtifactKind, Confidence, Edge, EdgeTarget, Extraction, Extractor, Node, NodeId,
    Result, Span, TargetRef,
};

pub struct MockExtractor;

impl MockExtractor {
    pub fn new() -> Self {
        MockExtractor
    }
}

impl Default for MockExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for MockExtractor {
    fn handles(&self, artifact: &Artifact) -> bool {
        // The mock happily "parses" any code file.
        matches!(artifact.kind, ArtifactKind::Code)
    }

    fn extract(&self, artifact: &Artifact, bytes: &[u8]) -> Result<Extraction> {
        // Deterministic id from the path (real derivation uses path + symbol).
        let file_id = format!("file:{}", artifact.path);
        let mut file_node = Node::new(&file_id, &artifact.path, "file");
        file_node.source_file = Some(artifact.path.clone());
        if let Some(lang) = &artifact.language {
            file_node.attrs.insert("language".into(), lang.clone());
        }

        let mut nodes = vec![file_node];
        let mut edges = Vec::new();

        // Dumb, language-agnostic, line-based heuristic purely to produce
        // Symbol edges (NO real AST): a `fn <name>` / `def <name>` line starts a
        // function node and becomes the "current" definition; any later
        // `<name>(` reference is attributed to that current function as an
        // UNRESOLVED `calls` edge for `resolve` to link.
        let text = String::from_utf8_lossy(bytes);
        let mut current_fn: Option<(NodeId, String)> = None;
        for (lineno, line) in text.lines().enumerate() {
            if let Some(name) = def_name(line) {
                let fid = format!("fn:{}:{}", artifact.path, name);
                let mut n = Node::new(&fid, &name, "function");
                n.source_file = Some(artifact.path.clone());
                n.source_span = Some(Span::line(lineno as u32 + 1));
                // The file "contains" this definition (already resolved).
                edges.push(Edge {
                    source: file_node_id(&artifact.path),
                    relation: CONTAINS.into(),
                    confidence: Confidence::Extracted,
                    target: EdgeTarget::Node(n.id.clone()),
                });
                current_fn = Some((n.id.clone(), name));
                nodes.push(n);
            } else if let (Some((caller_id, caller_name)), Some(callee)) =
                (current_fn.as_ref(), first_call(line))
            {
                if &callee != caller_name {
                    edges.push(Edge {
                        source: caller_id.clone(),
                        relation: CALLS.into(),
                        confidence: Confidence::Extracted,
                        target: EdgeTarget::Symbol(TargetRef::new(callee)),
                    });
                }
            }
        }

        Ok(Extraction {
            nodes,
            edges,
            ..Default::default()
        })
    }
}

fn file_node_id(path: &str) -> NodeId {
    NodeId::new(format!("file:{path}"))
}

fn def_name(line: &str) -> Option<String> {
    let l = line.trim_start();
    for kw in ["fn ", "def ", "function ", "func "] {
        if let Some(rest) = l.strip_prefix(kw) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn first_call(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let paren = line.find('(')?;
    let mut start = paren;
    while start > 0 {
        let c = bytes[start - 1];
        if (c as char).is_alphanumeric() || c == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    let name = &line[start..paren];
    if name.is_empty()
        || name
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(true)
    {
        return None;
    }
    // Skip keywords that look like calls.
    if matches!(
        name,
        "if" | "while" | "for" | "switch" | "match" | "fn" | "def"
    ) {
        return None;
    }
    Some(name.to_string())
}
