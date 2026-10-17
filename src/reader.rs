//! Reader mode: Mozilla's Readability pulls the article out of a page, and Bare shows it as
//! a plain, script-free page of its own.
//!
//! Scripts injected into the page report back through console messages starting with
//! [`PREFIX`]. The reader page is a `data:` document (an opaque origin) whose content
//! security policy blocks every script, frame, form and plugin, so nothing from the site runs
//! in it; only its text, links and images are shown.

use crate::block::js_string;

/// Marks console messages from the reader scripts.
pub const PREFIX: &str = "\u{2063}bare-reader\u{2063}";

/// Readability 0.6.0 (Apache-2.0), see assets/readability/LICENSE.md.
const READABILITY: &str = include_str!("../assets/readability/Readability.js");
const READERABLE: &str = include_str!("../assets/readability/Readability-readerable.js");

/// What a reader script reported.
#[derive(Debug, PartialEq)]
pub enum Report {
    /// Whether the page looks like an article (decides if the reader button shows).
    Readerable(bool),
    /// The extracted article (`None` if Readability found none), with the token it was asked
    /// for with: frames embedded in the page don't know it, so they can't answer instead.
    Article(String, Option<Article>),
}

#[derive(Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct Article {
    pub title: String,
    pub byline: String,
    pub site: String,
    pub lang: String,
    pub dir: String,
    /// The article as HTML, already cleaned up by Readability (no scripts or styles).
    pub content: String,
}

/// Run when a page finishes loading: reports whether it looks like an article.
pub fn check_script() -> String {
    format!(
        "(function(){{{READERABLE}\ntry{{console.log({prefix}+'readerable\\t'+(isProbablyReaderable(document)?'1':'0'))}}catch(e){{}}}})();",
        prefix = js_string(PREFIX)
    )
}

/// Run when the user opens reader view: reports the article, tagged with `token`.
pub fn extract_script(token: &str) -> String {
    format!(
        "(function(){{{READABILITY}\nvar r=null;try{{var a=new Readability(document.cloneNode(true)).parse();\
if(a&&a.content)r={{title:a.title||document.title||'',byline:a.byline||'',site:a.siteName||'',lang:a.lang||'',dir:a.dir||'',content:a.content}};}}catch(e){{}}\
console.log({tag}+JSON.stringify(r));}})();",
        tag = js_string(&format!("{PREFIX}article\t{token}\t"))
    )
}

/// Parses a console message from the reader scripts (`None` if it isn't one).
pub fn parse_report(message: &str) -> Option<Report> {
    let (kind, value) = message.strip_prefix(PREFIX)?.split_once('\t')?;
    match kind {
        "readerable" => Some(Report::Readerable(value == "1")),
        "article" => {
            let (token, json) = value.split_once('\t')?;
            Some(Report::Article(token.to_string(), serde_json::from_str::<Option<Article>>(json).ok().flatten()))
        }
        _ => None,
    }
}

/// Every script, frame, form, plugin and font is blocked; only images, media and the inline
/// style sheet load.
const POLICY: &str = "default-src 'none'; img-src https: data:; media-src https:; style-src 'unsafe-inline'; form-action 'none'; base-uri 'none'";

const STYLE: &str = r#"
:root{color-scheme:light dark;--bg:#fbfaf7;--fg:#1f1f22;--muted:#6b6b72;--line:#e3e1dc;--accent:#2563eb}
@media (prefers-color-scheme:dark){:root{--bg:#18181b;--fg:#e6e6ea;--muted:#9a9aa2;--line:#34343a;--accent:#60a5fa}}
html{background:var(--bg);color:var(--fg)}
body{margin:0;padding:48px 20px 96px;font:20px/1.65 Georgia,"Iowan Old Style","Palatino Linotype",serif}
main{max-width:38em;margin:0 auto}
header{border-bottom:1px solid var(--line);margin-bottom:1.6em;padding-bottom:1em}
.source{font:14px system-ui,sans-serif;color:var(--muted);text-decoration:none}
.source:hover{text-decoration:underline}
h1{font-size:1.9em;line-height:1.2;margin:.35em 0 .3em}
.byline{font:15px system-ui,sans-serif;color:var(--muted);margin:0}
a{color:var(--accent)}
img,video,svg,figure,picture{max-width:100%;height:auto}
figure{margin:1.4em 0}
figcaption{font:14px/1.4 system-ui,sans-serif;color:var(--muted);margin-top:.4em}
pre,code{font:15px/1.5 ui-monospace,Consolas,monospace}
pre{overflow-x:auto;padding:12px 14px;border:1px solid var(--line);border-radius:6px}
blockquote{margin:1.2em 0;padding-left:1em;border-left:3px solid var(--line);color:var(--muted)}
table{border-collapse:collapse;display:block;overflow-x:auto}
td,th{border:1px solid var(--line);padding:4px 8px}
h2,h3,h4{line-height:1.3}
"#;

/// The reader page for `article`, taken from the page at `source`.
pub fn page_html(article: &Article, source: &str) -> String {
    let lang = attr(article.lang.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(35).collect::<String>().as_str());
    let dir = if article.dir.eq_ignore_ascii_case("rtl") { "rtl" } else { "ltr" };
    let title = text(article.title.trim());
    let host = crate::url::host(source).unwrap_or(source);
    let link = if source.starts_with("https://") || source.starts_with("http://") {
        format!(r#"<a class="source" href="{}">{}</a>"#, attr(source), text(host))
    } else {
        String::new()
    };
    let byline: Vec<&str> = [article.byline.trim(), article.site.trim()].into_iter().filter(|s| !s.is_empty()).collect();
    let byline = if byline.is_empty() { String::new() } else { format!(r#"<p class="byline">{}</p>"#, text(&byline.join(" · "))) };
    format!(
        r#"<!doctype html><html lang="{lang}" dir="{dir}"><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="{POLICY}"><meta name="referrer" content="no-referrer"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><style>{STYLE}</style></head><body><main><header>{link}<h1>{title}</h1>{byline}</header><article>{content}</article></main></body></html>"#,
        content = article.content,
    )
}

/// The reader page as a `data:` URL.
pub fn page_url(article: &Article, source: &str) -> String {
    format!("data:text/html;charset=utf-8;base64,{}", base64(page_html(article, source).as_bytes()))
}

fn text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn attr(s: &str) -> String {
    text(s).replace('"', "&quot;").replace('\'', "&#39;")
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports() {
        assert_eq!(parse_report(&format!("{PREFIX}readerable\t1")), Some(Report::Readerable(true)));
        assert_eq!(parse_report(&format!("{PREFIX}readerable\t0")), Some(Report::Readerable(false)));
        assert_eq!(parse_report(&format!("{PREFIX}article\ttok\tnull")), Some(Report::Article("tok".into(), None)));
        assert_eq!(parse_report(&format!("{PREFIX}article\ttok\t{{broken")), Some(Report::Article("tok".into(), None)));
        assert_eq!(parse_report(&format!("{PREFIX}article\tnull")), None);
        let msg = format!("{PREFIX}article\ttok\t{}", r#"{"title":"T","content":"<p>x</p>","extra":1}"#);
        let Some(Report::Article(token, Some(a))) = parse_report(&msg) else {
            panic!("article not parsed");
        };
        assert_eq!(token, "tok");
        assert_eq!((a.title.as_str(), a.content.as_str(), a.byline.as_str()), ("T", "<p>x</p>", ""));
        assert_eq!(parse_report("readerable\t1"), None);
        assert_eq!(parse_report(&format!("{PREFIX}other\t1")), None);
    }

    #[test]
    fn page_escapes_and_locks_down() {
        let a = Article {
            title: "<script>alert(1)</script>".into(),
            byline: "A & B".into(),
            lang: "en\"><script>".into(),
            dir: "rtl".into(),
            content: "<p>Body</p>".into(),
            ..Default::default()
        };
        let html = page_html(&a, "https://news.test/a?x=\"1\"");
        assert!(html.starts_with(r#"<!doctype html><html lang="enscript" dir="rtl"><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none';"#));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("A &amp; B"));
        assert!(html.contains(r#"href="https://news.test/a?x=&quot;1&quot;""#));
        assert!(html.contains("<article><p>Body</p></article>"));
        // No link back for pages that aren't web pages.
        assert!(!page_html(&a, "javascript:alert(1)").contains("class=\"source\""));
    }

    #[test]
    fn base64_matches_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("é✓".as_bytes()), "w6ninJM=");
    }

    #[test]
    fn scripts_embed_readability() {
        assert!(check_script().contains("function isProbablyReaderable"));
        let extract = extract_script("tok123");
        assert!(extract.contains("function Readability("));
        assert!(extract.contains("bare-reader\u{2063}article\\u0009tok123\\u0009\""));
    }
}
