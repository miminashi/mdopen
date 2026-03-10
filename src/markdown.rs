use pulldown_cmark::TextMergeStream;
use pulldown_cmark::{html::push_html, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::iter::Iterator;

use crate::app_config::AppConfig;

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn to_tag_anchor(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

pub fn to_html(md: &str, #[allow(unused)] config: &AppConfig) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    options.insert(Options::ENABLE_MATH);
    options.insert(Options::ENABLE_GFM);
    options.insert(Options::ENABLE_MATH);

    let parser = Parser::new_ext(md, options);
    let parser = TextMergeStream::new(parser);

    // Mermaid code block processing: replace ```mermaid blocks with <pre class="mermaid">
    let mut inside_mermaid = false;

    let parser = parser.flat_map(move |event| match event {
        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(ref lang)))
            if lang.as_ref() == "mermaid" && config.enable_mermaid =>
        {
            inside_mermaid = true;
            vec![].into_iter()
        }
        Event::End(TagEnd::CodeBlock) if inside_mermaid => {
            inside_mermaid = false;
            vec![].into_iter()
        }
        Event::Text(ref text) if inside_mermaid => {
            let escaped = html_escape(text.as_ref());
            vec![Event::Html(
                format!("<pre class=\"mermaid\">{}</pre>", escaped).into(),
            )]
            .into_iter()
        }
        other => vec![other].into_iter(),
    });

    let mut inside_heading_level = false;

    let parser = parser.map(|event| match event {
        Event::Start(Tag::Heading { level, id, classes, attrs }) => {
            inside_heading_level = true;
            Event::Start(Tag::Heading { level, id, classes, attrs })
        }
        Event::End(TagEnd::Heading(level)) => {
            inside_heading_level = false;
            Event::End(TagEnd::Heading(level))
        }
        Event::Text(text) => {
            if inside_heading_level {
                let anchor = to_tag_anchor(&text);
                Event::Html(pulldown_cmark::CowStr::from(format!(r##"<a id="{anchor}" class="anchor" href="#{anchor}"><span class="octicon octicon-link"></span></a>{text}"##)))
            } else {
                Event::Text(text)
            }
        }
        _ => event,
    });

    #[cfg(feature = "syntax")]
    let parser: Box<dyn Iterator<Item = Event>> = if config.enable_syntax_highlight {
        Box::new(crate::syntax::map_highlighted_codeblocks::<'_>(parser))
    } else {
        Box::new(parser)
    };

    let mut html_output = String::new();
    push_html(&mut html_output, parser);

    html_output
}
