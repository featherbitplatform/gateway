//! Documentation pages embedded in the binary and served to agents as MCP
//! resources (`featherbit://docs/...`). The docs site's Markdown is the
//! single source of truth for plugin config keys, so agents read the same
//! pages humans do — minus Docusaurus frontmatter and JSX.

use regex::Regex;
use rust_embed::Embed;
use std::sync::OnceLock;

/// `website/docs/` subset compiled into the binary (~300 KB).
#[derive(Embed)]
#[folder = "website/docs/"]
#[include = "reference/plugins/*.md"]
#[include = "concepts/*.md"]
#[include = "reference/context-vars.md"]
#[include = "reference/conditions.md"]
#[include = "reference/templates.md"]
struct DocsAssets;

/// URI prefix of every documentation resource.
pub const DOCS_URI_PREFIX: &str = "featherbit://docs/";

/// Which docs directory a page lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocSection {
    Plugins,
    Concepts,
    Reference,
}

impl DocSection {
    fn slug(self) -> &'static str {
        match self {
            DocSection::Plugins => "plugins",
            DocSection::Concepts => "concepts",
            DocSection::Reference => "reference",
        }
    }
    fn dir(self) -> &'static str {
        match self {
            DocSection::Plugins => "reference/plugins/",
            DocSection::Concepts => "concepts/",
            DocSection::Reference => "reference/",
        }
    }
    fn parse(slug: &str) -> Option<Self> {
        match slug {
            "plugins" => Some(DocSection::Plugins),
            "concepts" => Some(DocSection::Concepts),
            "reference" => Some(DocSection::Reference),
            _ => None,
        }
    }
}

/// A listable page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocPage {
    pub uri: String,
    pub title: String,
    pub description: String,
}

/// The file behind a node type's page (`listener`/`client` share one).
fn plugin_file(node_type: &str) -> String {
    match node_type {
        "listener" | "client" => "listener-client".to_string(),
        other => other.to_string(),
    }
}

fn raw(section: DocSection, name: &str) -> Option<String> {
    if name.is_empty() || name == "index" || name.contains('/') || name.contains("..") {
        return None;
    }
    let path = format!("{}{}.md", section.dir(), name);
    // Normalize CRLF -> LF: the source files are LF in the repo, but a
    // checkout with `core.autocrlf=true` (common on Windows) embeds them
    // with CRLF, which would otherwise break every `\n`-anchored parse below.
    DocsAssets::get(&path).map(|f| String::from_utf8_lossy(&f.data).replace("\r\n", "\n"))
}

/// Frontmatter `title`/`description` (both may be empty).
fn frontmatter(md: &str) -> (String, String) {
    let mut title = String::new();
    let mut description = String::new();
    if let Some(rest) = md.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some(v) = line.strip_prefix("title:") {
                    title = v.trim().trim_matches('"').to_string();
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = v.trim().trim_matches('"').to_string();
                }
            }
        }
    }
    (title, description)
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\]\(((?:\.\./|\./)*)([A-Za-z0-9_./-]*?)([a-z0-9-]+)\.md(#[^)]*)?\)").unwrap()
    })
}

/// Strips frontmatter (keeping the title as an H1), `import` lines, and JSX
/// elements (single-line `<span …>…</span>` and multi-line `<Component … />`
/// blocks), and rewrites relative `.md` links to `featherbit://docs/…` URIs.
/// Fenced code blocks (opened/closed by a line whose trimmed form starts
/// with ```` ``` ````) pass through verbatim — no import/JSX/link
/// processing — since example bodies routinely contain `<Uppercase...>`
/// tokens (e.g. an RFC 5424 `<PRI>...` syslog frame) that would otherwise be
/// mistaken for an unterminated JSX block, silently swallowing everything
/// after it for the rest of the page.
///
/// Deviation from the literal brief: a captured link directory hint that
/// contains `guides/` is left as plain text instead of being rewritten. The
/// docs site's `guides/` pages are not embedded, and the brief's
/// directory-hint heuristic (`concepts/`/`plugins/`/`reference/`, else the
/// current section) has no way to represent "not embedded" — falling
/// through to "current section" would silently produce a broken
/// `featherbit://docs/<section>/...` URI for text like
/// `[stores](../../guides/admin-api.md#endpoint-reference)`, which appears
/// throughout the plugin pages. Leaving it as the original Markdown link
/// (dead outside the docs site, but not actively misleading) is preferable.
pub fn clean(md: &str, section: DocSection) -> String {
    let (title, _) = frontmatter(md);
    let body = if let Some(rest) = md.strip_prefix("---\n") {
        match rest.find("\n---") {
            Some(end) => &rest[end + 4..],
            None => md,
        }
    } else {
        md
    };

    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(&format!("# {title}\n"));
    }
    let mut in_jsx = false;
    let mut in_fence = false;
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_jsx {
            if t.ends_with("/>") || t.starts_with("</") {
                in_jsx = false;
            }
            continue;
        }
        if t.starts_with("import ") && t.ends_with(';') {
            continue;
        }
        if t.starts_with('<') && t.chars().nth(1).is_some_and(|c| c.is_ascii_uppercase()) {
            // <UiShot … /> possibly spanning lines.
            if !(t.ends_with("/>") || t.contains("</")) {
                in_jsx = true;
            }
            continue;
        }
        if t.starts_with("<span className=") && t.ends_with("</span>") {
            continue;
        }
        let rewritten = link_re().replace_all(line, |c: &regex::Captures| {
            let dirs = &c[2];
            let name = &c[3];
            if dirs.contains("guides/") {
                return c[0].to_string();
            }
            let target = if dirs.contains("concepts/") {
                DocSection::Concepts
            } else if dirs.contains("plugins/") {
                DocSection::Plugins
            } else if dirs.contains("reference/") {
                DocSection::Reference
            } else {
                section
            };
            format!("]({}{}/{})", DOCS_URI_PREFIX, target.slug(), name)
        });
        out.push_str(&rewritten);
        out.push('\n');
    }
    // Collapse runs of blank lines left by removed elements.
    let mut collapsed = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        collapsed.push_str(line);
        collapsed.push('\n');
    }
    collapsed
}

fn page(section: DocSection, name: &str) -> Option<String> {
    raw(section, name).map(|md| clean(&md, section))
}

/// The cleaned page for a node type.
pub fn plugin_page(node_type: &str) -> Option<String> {
    page(DocSection::Plugins, &plugin_file(node_type))
}

/// A concepts page by file stem (e.g. `supernodes`).
pub fn concept_page(name: &str) -> Option<String> {
    page(DocSection::Concepts, name)
}

/// A reference page by file stem (`context-vars`, `conditions`, `templates`).
pub fn reference_page(name: &str) -> Option<String> {
    page(DocSection::Reference, name)
}

/// Resolves a `featherbit://docs/{section}/{name}` URI.
pub fn read_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix(DOCS_URI_PREFIX)?;
    let (section, name) = rest.split_once('/')?;
    let section = DocSection::parse(section)?;
    match section {
        DocSection::Plugins => plugin_page(name),
        DocSection::Concepts => concept_page(name),
        DocSection::Reference => reference_page(name),
    }
}

/// Every page, for `resources/list`.
pub fn list_pages() -> Vec<DocPage> {
    let mut pages = Vec::new();
    for path in DocsAssets::iter() {
        let path = path.as_ref();
        let (section, stem) = if let Some(s) = path.strip_prefix("reference/plugins/") {
            (DocSection::Plugins, s)
        } else if let Some(s) = path.strip_prefix("concepts/") {
            (DocSection::Concepts, s)
        } else if let Some(s) = path.strip_prefix("reference/") {
            (DocSection::Reference, s)
        } else {
            continue;
        };
        let Some(stem) = stem.strip_suffix(".md") else {
            continue;
        };
        if stem == "index" {
            continue;
        }
        let Some(file) = DocsAssets::get(path) else {
            continue;
        };
        let md = String::from_utf8_lossy(&file.data).replace("\r\n", "\n");
        let (title, description) = frontmatter(&md);
        pages.push(DocPage {
            uri: format!("{}{}/{}", DOCS_URI_PREFIX, section.slug(), stem),
            title: if title.is_empty() {
                stem.to_string()
            } else {
                title
            },
            description,
        });
    }
    pages.sort_by(|a, b| a.uri.cmp(&b.uri));
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_type_has_a_page() {
        for entry in crate::admin::policies::plugin_catalog() {
            let t = entry["type"].as_str().unwrap();
            assert!(plugin_page(t).is_some(), "no docs page for node type '{t}'");
        }
    }

    #[test]
    fn plugin_page_is_cleaned() {
        let md = plugin_page("limit-count").unwrap();
        assert!(
            md.starts_with("# limit-count\n"),
            "{}",
            &md[..80.min(md.len())]
        );
        assert!(!md.contains("---\ntitle:"));
        assert!(!md.contains("plugin-chip"));
        assert!(md.contains("| `count` |"));
        assert!(
            md.contains("featherbit://docs/plugins/rate-limit"),
            "links rewritten"
        );
    }

    #[test]
    fn concept_page_drops_imports_and_jsx_blocks() {
        let md = concept_page("supernodes").unwrap();
        assert!(!md.contains("import UiShot"));
        assert!(!md.contains("<UiShot"));
        assert!(!md.contains("caption="));
        assert!(md.contains("featherbit://docs/concepts/policies-and-graphs"));
    }

    #[test]
    fn uri_mapping_and_listing() {
        assert!(read_uri("featherbit://docs/plugins/key-auth").is_some());
        assert!(read_uri("featherbit://docs/plugins/listener").is_some());
        assert!(read_uri("featherbit://docs/plugins/client").is_some());
        assert!(read_uri("featherbit://docs/reference/context-vars").is_some());
        assert!(read_uri("featherbit://docs/plugins/index").is_none());
        assert!(read_uri("featherbit://docs/nope/x").is_none());
        assert!(read_uri("featherbit://policies/x").is_none());
        let pages = list_pages();
        assert!(pages
            .iter()
            .any(|p| p.uri == "featherbit://docs/plugins/limit-count" && p.title == "limit-count"));
        assert!(pages.iter().all(|p| !p.uri.ends_with("/index")));
        assert!(
            pages
                .iter()
                .any(|p| p.uri == "featherbit://docs/concepts/supernodes"
                    && !p.description.is_empty())
        );
    }

    #[test]
    fn clean_handles_edge_cases() {
        let raw = "---\ntitle: T\ndescription: D\n---\n\nimport X from 'y';\n\n<span className=\"plugin-chip\">t</span>\n\nBody [link](./other.md) and [c](../../concepts/stores.md#a).\n\n<UiShot\n  name=\"x\"\n/>\n\nEnd\n";
        let out = clean(raw, DocSection::Plugins);
        assert_eq!(out, "# T\n\nBody [link](featherbit://docs/plugins/other) and [c](featherbit://docs/concepts/stores).\n\nEnd\n");
    }

    /// Guards against a mismap the directory-hint heuristic in `clean` can't
    /// resolve correctly: a `guides/` link (guides aren't embedded) must be
    /// left untouched rather than rewritten into a nonexistent
    /// `featherbit://docs/<current-section>/...` URI.
    #[test]
    fn guides_links_are_left_unchanged() {
        let raw = "---\ntitle: T\ndescription: D\n---\n\nSee the [Admin API](../../guides/admin-api.md#endpoint-reference) guide.\n";
        let out = clean(raw, DocSection::Plugins);
        assert!(out.contains("(../../guides/admin-api.md#endpoint-reference)"));
        assert!(!out.contains("featherbit://docs/plugins/admin-api"));
    }

    /// `clean` must never panic on any page actually embedded in the binary.
    #[test]
    fn clean_never_panics_on_any_embedded_page() {
        let pages = list_pages();
        assert!(
            pages.len() > 80,
            "sanity: expected the full docs set, got {}",
            pages.len()
        );
        for p in &pages {
            let content = read_uri(&p.uri).unwrap_or_else(|| panic!("no content for {}", p.uri));
            assert!(!content.is_empty(), "{} cleaned to empty content", p.uri);
        }
    }

    /// `syslog`'s raw page wraps an RFC 5424 example (`<PRI>...`) in a fenced
    /// code block. Before fenced blocks were exempted from JSX detection,
    /// that line tripped the multi-line-JSX heuristic (`<Uppercase...>` with
    /// no closing `/>`/`</` on the same line) and never found a closing tag,
    /// silently dropping the closing fence plus everything after it —
    /// the whole Configuration table included — for the rest of the file.
    #[test]
    fn syslog_page_survives_fenced_uppercase_tag() {
        let md = plugin_page("syslog").unwrap();
        assert!(
            md.contains("<PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID"),
            "fenced RFC 5424 example line dropped: {}",
            &md[..200.min(md.len())]
        );
        assert!(
            md.contains("## Configuration"),
            "Configuration heading dropped"
        );
        assert!(md.contains("| `host` |"), "Configuration table row dropped");
    }

    /// Structural invariant across every embedded plugin page: cleaning must
    /// never truncate a page partway through, so any page whose raw source
    /// has a `## Configuration` section must still have it after `clean()`.
    /// This is the regression guard for silent wholesale content loss (as
    /// happened with `syslog`) that `clean_never_panics_on_any_embedded_page`
    /// alone can't catch, since a truncated-but-non-empty page still passes
    /// that test.
    #[test]
    fn configuration_sections_survive_cleaning_for_every_plugin_page() {
        let mut checked = 0;
        for path in DocsAssets::iter() {
            let path = path.as_ref();
            let Some(stem) = path
                .strip_prefix("reference/plugins/")
                .and_then(|s| s.strip_suffix(".md"))
            else {
                continue;
            };
            if stem == "index" {
                continue;
            }
            let raw_md = raw(DocSection::Plugins, stem).unwrap();
            if raw_md.contains("## Configuration") {
                checked += 1;
                let cleaned = page(DocSection::Plugins, stem).unwrap();
                assert!(
                    cleaned.contains("## Configuration"),
                    "'{stem}' lost its Configuration section during cleaning"
                );
            }
        }
        assert!(
            checked > 80,
            "sanity: expected most plugin pages to have a Configuration section, got {checked}"
        );
    }
}
