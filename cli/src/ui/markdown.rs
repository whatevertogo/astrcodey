use std::borrow::Cow;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    capability::TerminalCapabilities,
    state::{
        WrappedLine, WrappedLineRewrapPolicy, WrappedLineStyle, WrappedSpan, WrappedSpanStyle,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum InlineChunk {
    Plain(String),
    Styled(WrappedSpanStyle, String),
}

impl InlineChunk {
    fn plain(text: impl Into<String>) -> Self {
        Self::Plain(text.into())
    }

    fn styled(style: WrappedSpanStyle, text: impl Into<String>) -> Self {
        Self::Styled(style, text.into())
    }

    fn style(&self) -> Option<WrappedSpanStyle> {
        match self {
            Self::Plain(_) => None,
            Self::Styled(style, _) => Some(*style),
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::Plain(text) | Self::Styled(_, text) => text.as_str(),
        }
    }

    fn append_text(&mut self, text: &str) {
        match self {
            Self::Plain(current) | Self::Styled(_, current) => current.push_str(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InlineNode {
    Text(String),
    Code(String),
    SoftBreak,
    HardBreak,
    Styled(WrappedSpanStyle, Vec<InlineNode>),
    Link {
        label: Vec<InlineNode>,
        destination: String,
    },
    Image {
        alt: Vec<InlineNode>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BlockNode {
    Paragraph(Vec<InlineNode>),
    Heading {
        level: usize,
        content: Vec<InlineNode>,
    },
    BlockQuote(Vec<BlockNode>),
    List {
        start: Option<u64>,
        items: Vec<Vec<BlockNode>>,
    },
    CodeBlock {
        info: Option<String>,
        content: String,
    },
    Table {
        headers: Vec<Vec<InlineNode>>,
        rows: Vec<Vec<Vec<InlineNode>>>,
    },
    Rule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineTerminator {
    Paragraph,
    Heading,
    Emphasis,
    Strong,
    Link,
    Image,
    TableCell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockTerminator {
    BlockQuote,
    Item,
}

#[derive(Debug, Clone, Copy)]
struct TableChars<'a> {
    top_left: &'a str,
    top_mid: &'a str,
    top_right: &'a str,
    mid_left: &'a str,
    mid_mid: &'a str,
    mid_right: &'a str,
    bottom_left: &'a str,
    bottom_mid: &'a str,
    bottom_right: &'a str,
    horizontal: &'a str,
    vertical: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RichTableRow {
    cells: Vec<Vec<InlineChunk>>,
    is_separator: bool,
    is_header: bool,
}

pub(crate) fn render_markdown_lines(
    text: &str,
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    if width == 0 {
        return vec![WrappedLine::plain(style, String::new())];
    }

    let segments = split_markdown_segments(text);
    let mut output = Vec::new();

    for segment in segments {
        match segment {
            MarkdownSegment::Blank => output.push(WrappedLine::plain(style, String::new())),
            MarkdownSegment::Text(block) => {
                let nodes = parse_markdown_to_blocks(block.as_str());
                output.extend(layout_blocks(&nodes, width, capabilities, style));
            },
            MarkdownSegment::Preformatted(lines) => output.extend(render_preserved_lines(
                &strip_pseudo_fence_lines(lines),
                style,
                WrappedSpanStyle::TextArt,
            )),
        }
    }

    if output.is_empty() {
        output.push(WrappedLine::plain(style, String::new()));
    }
    output
}

#[cfg(test)]
pub(crate) fn wrap_text(
    text: &str,
    width: usize,
    capabilities: TerminalCapabilities,
) -> Vec<String> {
    render_markdown_lines(text, width, capabilities, WrappedLineStyle::Plain)
        .into_iter()
        .map(|line| line.text())
        .collect()
}

pub(crate) fn render_literal_text(
    text: &str,
    width: usize,
    capabilities: TerminalCapabilities,
) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut output = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            output.push(String::new());
            continue;
        }
        output.extend(wrap_paragraph(trimmed, width, capabilities));
    }

    if output.is_empty() {
        output.push(String::new());
    }
    output
}

pub(crate) fn render_preformatted_block(
    body: &str,
    _width: usize,
    _capabilities: TerminalCapabilities,
) -> Vec<String> {
    let mut lines = body
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
pub(crate) fn flatten_inline_text(text: &str) -> String {
    flatten_inline_markdown(text)
}

fn render_blocks(
    blocks: &[BlockNode],
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    let mut output = Vec::new();
    for block in blocks {
        output.extend(render_block(block, width, capabilities, style));
    }
    output
}

fn parse_markdown_to_blocks(text: &str) -> Vec<BlockNode> {
    parse_markdown_segment(text)
}

fn layout_blocks(
    blocks: &[BlockNode],
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    render_blocks(blocks, width, capabilities, style)
}

#[cfg(test)]
fn flatten_inline_markdown(text: &str) -> String {
    flatten_inline_chunks(render_inline_chunks(&parse_inline_segment(text)))
}

fn render_block(
    block: &BlockNode,
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    match block {
        BlockNode::Paragraph(content) => render_inline_block(content, width, capabilities, style),
        BlockNode::Heading { level, content } => {
            render_heading_block(*level, content, width, capabilities, style)
        },
        BlockNode::BlockQuote(blocks) => {
            let quoted = render_blocks(
                blocks,
                width
                    .saturating_sub(display_width(quote_prefix(capabilities)))
                    .max(1),
                capabilities,
                style,
            );
            prefix_lines(
                quoted,
                vec![InlineChunk::styled(
                    WrappedSpanStyle::QuoteMarker,
                    quote_prefix(capabilities),
                )],
                vec![InlineChunk::styled(
                    WrappedSpanStyle::QuoteMarker,
                    quote_prefix(capabilities),
                )],
                style,
            )
        },
        BlockNode::List { start, items } => {
            render_list_block(items, *start, width, capabilities, style)
        },
        BlockNode::CodeBlock { info, content } => render_code_block(
            info.as_deref(),
            content.as_str(),
            width,
            capabilities,
            style,
        ),
        BlockNode::Table { headers, rows } => {
            render_table_from_nodes(headers, rows, width, capabilities, style)
        },
        BlockNode::Rule => vec![
            rich_line(
                style,
                vec![InlineChunk::styled(
                    WrappedSpanStyle::HeadingRule,
                    render_horizontal_rule(width, capabilities),
                )],
            )
            .with_rewrap_policy(WrappedLineRewrapPolicy::PreserveAndCrop),
        ],
    }
}

fn render_inline_block(
    content: &[InlineNode],
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    split_inline_sections(content)
        .into_iter()
        .flat_map(|section| {
            wrap_inline_chunks(render_inline_chunks(&section), width, capabilities)
                .into_iter()
                .map(|chunks| rich_line(style, chunks))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn render_heading_block(
    level: usize,
    content: &[InlineNode],
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    let mut chunks = render_inline_chunks(content);
    apply_style_to_plain_chunks(&mut chunks, WrappedSpanStyle::Heading);
    let text = flatten_inline_chunks(chunks.clone());
    let mut lines = wrap_inline_chunks(chunks, width, capabilities)
        .into_iter()
        .map(|chunks| rich_line(style, chunks))
        .collect::<Vec<_>>();
    if level <= 2 {
        let underline = render_heading_rule(level, text.as_str(), width, capabilities);
        lines.push(rich_line(
            style,
            vec![InlineChunk::styled(
                WrappedSpanStyle::HeadingRule,
                underline,
            )],
        ));
    }
    lines
}

fn render_list_block(
    items: &[Vec<BlockNode>],
    start: Option<u64>,
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    let mut output = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let marker = match start {
            Some(value) => format!("{}. ", value + index as u64),
            None => "- ".to_string(),
        };
        let indent = " ".repeat(display_width(marker.as_str()));
        let rendered = render_blocks(
            item,
            width.saturating_sub(display_width(marker.as_str())).max(1),
            capabilities,
            style,
        );
        output.extend(prefix_lines(
            rendered,
            vec![InlineChunk::styled(
                WrappedSpanStyle::ListMarker,
                marker.clone(),
            )],
            vec![InlineChunk::plain(indent)],
            style,
        ));
    }
    output
}

fn render_code_block(
    _info: Option<&str>,
    content: &str,
    _width: usize,
    _capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    let lines = split_preserved_block_lines(content);
    let text_art = should_use_preformatted_fallback(content);
    render_preserved_lines(
        &lines,
        style,
        if text_art {
            WrappedSpanStyle::TextArt
        } else {
            WrappedSpanStyle::CodeText
        },
    )
}

fn render_table_from_nodes(
    headers: &[Vec<InlineNode>],
    rows: &[Vec<Vec<InlineNode>>],
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    render_rich_table_rows(headers, rows, width, capabilities, style)
}

fn split_inline_sections(content: &[InlineNode]) -> Vec<Vec<InlineNode>> {
    let mut sections = Vec::new();
    let mut current = Vec::new();

    for node in content {
        if matches!(node, InlineNode::HardBreak) {
            sections.push(std::mem::take(&mut current));
            continue;
        }
        current.push(node.clone());
    }

    if !current.is_empty() || sections.is_empty() {
        sections.push(current);
    }
    sections
}

fn render_inline_chunks(nodes: &[InlineNode]) -> Vec<InlineChunk> {
    let mut output = Vec::new();
    for node in nodes {
        match node {
            InlineNode::Text(text) => push_chunk(&mut output, InlineChunk::plain(text.clone())),
            InlineNode::Code(text) => push_chunk(
                &mut output,
                InlineChunk::styled(WrappedSpanStyle::InlineCode, text.clone()),
            ),
            InlineNode::SoftBreak => push_chunk(&mut output, InlineChunk::plain(" ")),
            InlineNode::HardBreak => {},
            InlineNode::Styled(style, children) => {
                let mut nested = render_inline_chunks(children);
                apply_style_to_plain_chunks(&mut nested, *style);
                output.extend(nested);
            },
            InlineNode::Link { label, destination } => {
                let label = flatten_inline_chunks(render_inline_chunks(label));
                let rendered =
                    if destination.is_empty() || label.is_empty() || label == *destination {
                        label
                    } else {
                        format!("{label} ({destination})")
                    };
                push_chunk(
                    &mut output,
                    InlineChunk::styled(WrappedSpanStyle::Link, rendered),
                );
            },
            InlineNode::Image { alt } => {
                let alt = flatten_inline_chunks(render_inline_chunks(alt));
                let rendered = if alt.is_empty() {
                    "[image]".to_string()
                } else {
                    alt
                };
                push_chunk(&mut output, InlineChunk::plain(rendered));
            },
        }
    }
    output
}

fn prefix_lines(
    lines: Vec<WrappedLine>,
    first_prefix: Vec<InlineChunk>,
    rest_prefix: Vec<InlineChunk>,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    if lines.is_empty() {
        return vec![rich_line(style, first_prefix)];
    }

    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 {
                first_prefix.clone()
            } else {
                rest_prefix.clone()
            };
            prepend_inline_chunks(line, prefix)
        })
        .collect()
}

fn rich_line(style: WrappedLineStyle, chunks: Vec<InlineChunk>) -> WrappedLine {
    WrappedLine::from_spans(
        style,
        chunks
            .into_iter()
            .filter_map(|chunk| match chunk {
                InlineChunk::Plain(text) if text.is_empty() => None,
                InlineChunk::Plain(text) => Some(WrappedSpan::plain(text)),
                InlineChunk::Styled(_, text) if text.is_empty() => None,
                InlineChunk::Styled(span_style, text) => {
                    Some(WrappedSpan::styled(span_style, text))
                },
            })
            .collect(),
    )
}

fn prepend_inline_chunks(mut line: WrappedLine, prefix: Vec<InlineChunk>) -> WrappedLine {
    if prefix.is_empty() {
        return line;
    }
    let mut spans = prefix
        .into_iter()
        .filter_map(|chunk| match chunk {
            InlineChunk::Plain(text) if text.is_empty() => None,
            InlineChunk::Plain(text) => Some(WrappedSpan::plain(text)),
            InlineChunk::Styled(_, text) if text.is_empty() => None,
            InlineChunk::Styled(style, text) => Some(WrappedSpan::styled(style, text)),
        })
        .collect::<Vec<_>>();
    spans.extend(line.spans);
    line.spans = spans;
    line
}

fn push_chunk(output: &mut Vec<InlineChunk>, chunk: InlineChunk) {
    if chunk.text().is_empty() {
        return;
    }
    if let Some(last) = output.last_mut() {
        if last.style() == chunk.style() {
            last.append_text(chunk.text());
            return;
        }
    }
    output.push(chunk);
}

fn apply_style_to_plain_chunks(chunks: &mut [InlineChunk], style: WrappedSpanStyle) {
    for chunk in chunks {
        if let InlineChunk::Plain(text) = chunk {
            *chunk = InlineChunk::styled(style, text.clone());
        }
    }
}

fn render_heading_rule(
    level: usize,
    text: &str,
    width: usize,
    capabilities: TerminalCapabilities,
) -> String {
    let glyph = match (level, capabilities.ascii_only()) {
        (1, false) => "═",
        (1, true) => "=",
        (_, false) => "─",
        (_, true) => "-",
    };
    glyph.repeat(display_width(text).clamp(3, width.clamp(3, 48)))
}

fn quote_prefix(capabilities: TerminalCapabilities) -> &'static str {
    if capabilities.ascii_only() {
        "| "
    } else {
        "│ "
    }
}

fn render_horizontal_rule(width: usize, capabilities: TerminalCapabilities) -> String {
    let glyph = if capabilities.ascii_only() {
        "-"
    } else {
        "─"
    };
    glyph.repeat(width.clamp(3, 48))
}

fn parse_markdown_segment(text: &str) -> Vec<BlockNode> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(text, options).collect::<Vec<_>>();
    let mut index = 0;
    parse_blocks_until(&events, &mut index, None)
}

fn parse_blocks_until<'a>(
    events: &[Event<'a>],
    index: &mut usize,
    terminator: Option<BlockTerminator>,
) -> Vec<BlockNode> {
    let mut blocks = Vec::new();

    while *index < events.len() {
        match &events[*index] {
            Event::End(tag)
                if terminator.is_some_and(|term| matches_block_terminator(tag, term)) =>
            {
                *index += 1;
                break;
            },
            Event::Start(Tag::Paragraph) => {
                *index += 1;
                blocks.push(BlockNode::Paragraph(parse_inlines_until(
                    events,
                    index,
                    InlineTerminator::Paragraph,
                )));
            },
            Event::Start(Tag::Heading(level, ..)) => {
                let level = heading_level(*level);
                *index += 1;
                blocks.push(BlockNode::Heading {
                    level,
                    content: parse_inlines_until(events, index, InlineTerminator::Heading),
                });
            },
            Event::Start(Tag::BlockQuote) => {
                *index += 1;
                blocks.push(BlockNode::BlockQuote(parse_blocks_until(
                    events,
                    index,
                    Some(BlockTerminator::BlockQuote),
                )));
            },
            Event::Start(Tag::List(start)) => {
                let start = *start;
                *index += 1;
                let mut items = Vec::new();
                while *index < events.len() {
                    match &events[*index] {
                        Event::Start(Tag::Item) => {
                            *index += 1;
                            items.push(parse_blocks_until(
                                events,
                                index,
                                Some(BlockTerminator::Item),
                            ));
                        },
                        Event::End(Tag::List(_)) => {
                            *index += 1;
                            break;
                        },
                        _ => *index += 1,
                    }
                }
                blocks.push(BlockNode::List { start, items });
            },
            Event::Start(Tag::CodeBlock(kind)) => {
                let info = match kind {
                    CodeBlockKind::Fenced(info) => Some(info.to_string()),
                    CodeBlockKind::Indented => None,
                };
                *index += 1;
                blocks.push(BlockNode::CodeBlock {
                    info,
                    content: parse_code_block(events, index),
                });
            },
            Event::Start(Tag::Table(_)) => {
                *index += 1;
                blocks.push(parse_table(events, index));
            },
            Event::Rule => {
                blocks.push(BlockNode::Rule);
                *index += 1;
            },
            Event::Html(html) | Event::Text(html) => {
                let text = html.to_string();
                *index += 1;
                if !text.trim().is_empty() {
                    blocks.push(BlockNode::Paragraph(vec![InlineNode::Text(text)]));
                }
            },
            Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::TaskListMarker(_)
            | Event::Start(Tag::Emphasis)
            | Event::Start(Tag::Strong)
            | Event::Start(Tag::Strikethrough)
            | Event::Start(Tag::Link(..))
            | Event::Start(Tag::Image(..)) => blocks.push(BlockNode::Paragraph(
                parse_inline_flow_until_block_boundary(events, index, terminator),
            )),
            _ => *index += 1,
        }
    }

    blocks
}

fn parse_inlines_until<'a>(
    events: &[Event<'a>],
    index: &mut usize,
    terminator: InlineTerminator,
) -> Vec<InlineNode> {
    let mut nodes = Vec::new();

    while *index < events.len() {
        match &events[*index] {
            Event::End(tag) if matches_inline_terminator(tag, terminator) => {
                *index += 1;
                break;
            },
            Event::Text(text) => {
                push_inline_text(&mut nodes, text.as_ref());
                *index += 1;
            },
            Event::Code(text) => {
                nodes.push(InlineNode::Code(text.to_string()));
                *index += 1;
            },
            Event::SoftBreak => {
                nodes.push(InlineNode::SoftBreak);
                *index += 1;
            },
            Event::HardBreak => {
                nodes.push(InlineNode::HardBreak);
                *index += 1;
            },
            Event::TaskListMarker(checked) => {
                push_inline_text(&mut nodes, if *checked { "[x] " } else { "[ ] " });
                *index += 1;
            },
            Event::Start(Tag::Emphasis) => {
                *index += 1;
                nodes.push(InlineNode::Styled(
                    WrappedSpanStyle::Emphasis,
                    parse_inlines_until(events, index, InlineTerminator::Emphasis),
                ));
            },
            Event::Start(Tag::Strong) => {
                *index += 1;
                nodes.push(InlineNode::Styled(
                    WrappedSpanStyle::Strong,
                    parse_inlines_until(events, index, InlineTerminator::Strong),
                ));
            },
            Event::Start(Tag::Strikethrough) => {
                *index += 1;
                nodes.push(InlineNode::Styled(
                    WrappedSpanStyle::Emphasis,
                    parse_inlines_until(events, index, InlineTerminator::Emphasis),
                ));
            },
            Event::Start(Tag::Link(_, destination, _)) => {
                let destination = destination.to_string();
                *index += 1;
                nodes.push(InlineNode::Link {
                    label: parse_inlines_until(events, index, InlineTerminator::Link),
                    destination,
                });
            },
            Event::Start(Tag::Image(_, _, _)) => {
                *index += 1;
                nodes.push(InlineNode::Image {
                    alt: parse_inlines_until(events, index, InlineTerminator::Image),
                });
            },
            Event::Html(html) => {
                push_inline_text(&mut nodes, html.as_ref());
                *index += 1;
            },
            _ => *index += 1,
        }
    }

    nodes
}

fn parse_inline_flow_until_block_boundary<'a>(
    events: &[Event<'a>],
    index: &mut usize,
    terminator: Option<BlockTerminator>,
) -> Vec<InlineNode> {
    let mut nodes = Vec::new();

    while *index < events.len() {
        match &events[*index] {
            Event::End(tag)
                if terminator.is_some_and(|term| matches_block_terminator(tag, term)) =>
            {
                break;
            },
            Event::Start(Tag::Paragraph)
            | Event::Start(Tag::Heading(..))
            | Event::Start(Tag::BlockQuote)
            | Event::Start(Tag::List(_))
            | Event::Start(Tag::CodeBlock(_))
            | Event::Start(Tag::Table(_))
            | Event::Rule => break,
            Event::Text(text) => {
                push_inline_text(&mut nodes, text.as_ref());
                *index += 1;
            },
            Event::Code(text) => {
                nodes.push(InlineNode::Code(text.to_string()));
                *index += 1;
            },
            Event::SoftBreak => {
                nodes.push(InlineNode::SoftBreak);
                *index += 1;
            },
            Event::HardBreak => {
                nodes.push(InlineNode::HardBreak);
                *index += 1;
            },
            Event::TaskListMarker(checked) => {
                push_inline_text(&mut nodes, if *checked { "[x] " } else { "[ ] " });
                *index += 1;
            },
            Event::Start(Tag::Emphasis) => {
                *index += 1;
                nodes.push(InlineNode::Styled(
                    WrappedSpanStyle::Emphasis,
                    parse_inlines_until(events, index, InlineTerminator::Emphasis),
                ));
            },
            Event::Start(Tag::Strong) => {
                *index += 1;
                nodes.push(InlineNode::Styled(
                    WrappedSpanStyle::Strong,
                    parse_inlines_until(events, index, InlineTerminator::Strong),
                ));
            },
            Event::Start(Tag::Strikethrough) => {
                *index += 1;
                nodes.push(InlineNode::Styled(
                    WrappedSpanStyle::Emphasis,
                    parse_inlines_until(events, index, InlineTerminator::Emphasis),
                ));
            },
            Event::Start(Tag::Link(_, destination, _)) => {
                let destination = destination.to_string();
                *index += 1;
                nodes.push(InlineNode::Link {
                    label: parse_inlines_until(events, index, InlineTerminator::Link),
                    destination,
                });
            },
            Event::Start(Tag::Image(_, _, _)) => {
                *index += 1;
                nodes.push(InlineNode::Image {
                    alt: parse_inlines_until(events, index, InlineTerminator::Image),
                });
            },
            Event::Html(html) => {
                push_inline_text(&mut nodes, html.as_ref());
                *index += 1;
            },
            _ => *index += 1,
        }
    }

    nodes
}

fn parse_code_block<'a>(events: &[Event<'a>], index: &mut usize) -> String {
    let mut content = String::new();

    while *index < events.len() {
        match &events[*index] {
            Event::End(Tag::CodeBlock(_)) => {
                *index += 1;
                break;
            },
            Event::Text(text) | Event::Html(text) => {
                content.push_str(text.as_ref());
                *index += 1;
            },
            Event::SoftBreak | Event::HardBreak => {
                content.push('\n');
                *index += 1;
            },
            _ => *index += 1,
        }
    }

    content
}

fn parse_table<'a>(events: &[Event<'a>], index: &mut usize) -> BlockNode {
    let mut headers = Vec::new();
    let mut rows = Vec::new();

    while *index < events.len() {
        match &events[*index] {
            Event::Start(Tag::TableHead) => {
                *index += 1;
                headers = parse_table_head(events, index);
            },
            Event::Start(Tag::TableRow) => rows.push(parse_table_row(events, index)),
            Event::End(Tag::Table(_)) => {
                *index += 1;
                break;
            },
            _ => *index += 1,
        }
    }

    BlockNode::Table { headers, rows }
}

fn parse_table_head<'a>(events: &[Event<'a>], index: &mut usize) -> Vec<Vec<InlineNode>> {
    let mut cells = Vec::new();

    while *index < events.len() {
        match &events[*index] {
            Event::Start(Tag::TableCell) => {
                *index += 1;
                cells.push(parse_inlines_until(
                    events,
                    index,
                    InlineTerminator::TableCell,
                ));
            },
            Event::End(Tag::TableHead) => {
                *index += 1;
                break;
            },
            _ => *index += 1,
        }
    }

    cells
}

fn parse_table_row<'a>(events: &[Event<'a>], index: &mut usize) -> Vec<Vec<InlineNode>> {
    let mut cells = Vec::new();
    *index += 1;

    while *index < events.len() {
        match &events[*index] {
            Event::Start(Tag::TableCell) => {
                *index += 1;
                cells.push(parse_inlines_until(
                    events,
                    index,
                    InlineTerminator::TableCell,
                ));
            },
            Event::End(Tag::TableRow) => {
                *index += 1;
                break;
            },
            _ => *index += 1,
        }
    }

    cells
}

fn push_inline_text(nodes: &mut Vec<InlineNode>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(InlineNode::Text(existing)) = nodes.last_mut() {
        existing.push_str(text);
    } else {
        nodes.push(InlineNode::Text(text.to_string()));
    }
}

fn matches_inline_terminator(tag: &Tag<'_>, terminator: InlineTerminator) -> bool {
    match terminator {
        InlineTerminator::Paragraph => matches!(tag, Tag::Paragraph),
        InlineTerminator::Heading => matches!(tag, Tag::Heading(..)),
        InlineTerminator::Emphasis => matches!(tag, Tag::Emphasis | Tag::Strikethrough),
        InlineTerminator::Strong => matches!(tag, Tag::Strong),
        InlineTerminator::Link => matches!(tag, Tag::Link(..)),
        InlineTerminator::Image => matches!(tag, Tag::Image(..)),
        InlineTerminator::TableCell => matches!(tag, Tag::TableCell),
    }
}

fn matches_block_terminator(tag: &Tag<'_>, terminator: BlockTerminator) -> bool {
    match terminator {
        BlockTerminator::BlockQuote => matches!(tag, Tag::BlockQuote),
        BlockTerminator::Item => matches!(tag, Tag::Item),
    }
}

fn heading_level(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkdownSegment {
    Blank,
    Text(String),
    Preformatted(Vec<String>),
}

fn split_markdown_segments(text: &str) -> Vec<MarkdownSegment> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    let mut in_fence = false;
    let mut fence_marker = "";

    for line in text.lines() {
        let trimmed = line.trim_end();
        let fence = fence_delimiter(trimmed);
        if !in_fence && trimmed.is_empty() {
            if !current.is_empty() {
                segments.push(segment_from_lines(&current));
                current.clear();
            }
            segments.push(MarkdownSegment::Blank);
            continue;
        }

        current.push(trimmed.to_string());
        if let Some(marker) = fence {
            if in_fence && trimmed.trim_start().starts_with(fence_marker) {
                in_fence = false;
                fence_marker = "";
            } else if !in_fence {
                in_fence = true;
                fence_marker = marker;
            }
        }
    }

    if !current.is_empty() {
        segments.push(segment_from_lines(&current));
    }

    if segments.is_empty() {
        segments.push(MarkdownSegment::Text(String::new()));
    }

    segments
}

fn segment_from_lines(lines: &[String]) -> MarkdownSegment {
    let block = lines.join("\n");
    if should_use_preformatted_fallback(block.as_str()) {
        MarkdownSegment::Preformatted(lines.to_vec())
    } else {
        MarkdownSegment::Text(block)
    }
}

fn should_use_preformatted_fallback(block: &str) -> bool {
    let lines = block
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() || looks_like_markdown_table(&lines) {
        return false;
    }

    let boxy = lines
        .iter()
        .filter(|line| contains_box_drawing(line))
        .count();
    let treey = lines
        .iter()
        .filter(|line| contains_tree_connectors(line))
        .count();
    let shelly = lines
        .iter()
        .filter(|line| {
            line.contains("=>") || line.contains("::") || line.contains("  ") || line.contains('\t')
        })
        .count();

    boxy >= 2
        || treey >= 2
        || lines
            .iter()
            .any(|line| contains_tree_connectors(line) && line.split_whitespace().count() >= 4)
        || (boxy >= 1 && shelly >= 2)
}

fn looks_like_markdown_table(lines: &[&str]) -> bool {
    if lines.len() < 2 {
        return false;
    }
    is_table_line(lines[0]) && is_table_line(lines[1])
}

fn contains_box_drawing(line: &str) -> bool {
    line.chars().any(|ch| {
        matches!(
            ch,
            '│' | '─'
                | '┌'
                | '┐'
                | '└'
                | '┘'
                | '├'
                | '┤'
                | '┬'
                | '┴'
                | '┼'
                | '═'
                | '╭'
                | '╮'
                | '╰'
                | '╯'
        )
    })
}

fn contains_tree_connectors(line: &str) -> bool {
    line.contains("├")
        || line.contains("└")
        || line.contains("│")
        || line.contains("┬")
        || line.contains("┴")
        || line.contains("┼")
        || line.contains("──")
        || line.contains("|--")
        || line.contains("+-")
}

fn strip_pseudo_fence_lines(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() >= 3
        && is_pseudo_fence_line(&lines[0])
        && lines.last().is_some_and(|line| is_pseudo_fence_line(line))
    {
        lines.remove(0);
        lines.pop();
    }
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn is_pseudo_fence_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 2 && trimmed.chars().all(|ch| ch == '`' || ch == '~')
}

fn fence_delimiter(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn is_table_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
}

#[cfg(test)]
fn parse_inline_segment(text: &str) -> Vec<InlineNode> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let markdown = format!("{text}\n");
    let events = Parser::new_ext(markdown.as_str(), options).collect::<Vec<_>>();
    let mut index = 0;

    while index < events.len() {
        if matches!(events[index], Event::Start(Tag::Paragraph)) {
            index += 1;
            return parse_inlines_until(&events, &mut index, InlineTerminator::Paragraph);
        }
        index += 1;
    }

    vec![InlineNode::Text(text.to_string())]
}

fn wrap_paragraph(text: &str, width: usize, capabilities: TerminalCapabilities) -> Vec<String> {
    wrap_with_prefix(text, width, capabilities, "", "")
}

fn wrap_with_prefix(
    text: &str,
    width: usize,
    capabilities: TerminalCapabilities,
    first_prefix: &str,
    subsequent_prefix: &str,
) -> Vec<String> {
    let tokens = text.split_whitespace().collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut current_prefix = first_prefix;
    let mut current = current_prefix.to_string();
    let mut current_width = display_width(current_prefix);
    let first_prefix_width = display_width(first_prefix);
    let subsequent_prefix_width = display_width(subsequent_prefix);
    let first_available = width.saturating_sub(first_prefix_width).max(1);
    let subsequent_available = width.saturating_sub(subsequent_prefix_width).max(1);
    let mut current_available = first_available;

    for token in tokens {
        for chunk in split_token_by_width(token, current_available.max(1), capabilities) {
            let chunk_width = display_width(chunk.as_ref());
            let needs_space = current_width > display_width(current_prefix);
            let space_width = usize::from(needs_space);

            if current_width > display_width(current_prefix)
                && current_width + space_width + chunk_width > width
            {
                lines.push(current);
                current_prefix = subsequent_prefix;
                current = current_prefix.to_string();
                current_width = display_width(current_prefix);
                current_available = subsequent_available;
            }

            if current_width > display_width(current_prefix) {
                current.push(' ');
                current_width += 1;
            }

            current.push_str(chunk.as_ref());
            current_width += chunk_width;

            if current_width == display_width(current_prefix) {
                current_available = subsequent_available;
            }
        }
    }

    if current_width > display_width(current_prefix) || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn render_preserved_lines(
    lines: &[String],
    style: WrappedLineStyle,
    span_style: WrappedSpanStyle,
) -> Vec<WrappedLine> {
    let lines = if lines.is_empty() {
        vec![String::new()]
    } else {
        lines.to_vec()
    };
    lines
        .into_iter()
        .map(|line| {
            rich_line(style, vec![InlineChunk::styled(span_style, line)])
                .with_rewrap_policy(WrappedLineRewrapPolicy::PreserveAndCrop)
        })
        .collect()
}

fn split_preserved_block_lines(content: &str) -> Vec<String> {
    if content.is_empty() {
        vec![String::new()]
    } else {
        content
            .trim_end_matches('\n')
            .split('\n')
            .map(ToString::to_string)
            .collect()
    }
}

fn render_rich_table_rows(
    headers: &[Vec<InlineNode>],
    rows: &[Vec<Vec<InlineNode>>],
    width: usize,
    capabilities: TerminalCapabilities,
    style: WrappedLineStyle,
) -> Vec<WrappedLine> {
    let mut rich_rows = Vec::new();
    if !headers.is_empty() {
        rich_rows.push(RichTableRow {
            cells: headers
                .iter()
                .map(|cell| render_inline_chunks(cell))
                .collect(),
            is_separator: false,
            is_header: true,
        });
        rich_rows.push(RichTableRow {
            cells: vec![Vec::new(); headers.len()],
            is_separator: true,
            is_header: false,
        });
    }
    rich_rows.extend(rows.iter().map(|row| RichTableRow {
        cells: row.iter().map(|cell| render_inline_chunks(cell)).collect(),
        is_separator: false,
        is_header: false,
    }));

    let col_count = rich_rows
        .iter()
        .map(|row| row.cells.len())
        .max()
        .unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    let col_widths = compute_rich_table_widths(&rich_rows, width);
    let chars = table_chars(capabilities);
    let mut rendered = vec![
        rich_line(
            style,
            border_chunks(
                &col_widths,
                chars.top_left,
                chars.top_mid,
                chars.top_right,
                chars.horizontal,
            ),
        )
        .with_rewrap_policy(WrappedLineRewrapPolicy::PreserveAndCrop),
    ];
    for row in &rich_rows {
        if row.is_separator {
            rendered.push(
                rich_line(
                    style,
                    border_chunks(
                        &col_widths,
                        chars.mid_left,
                        chars.mid_mid,
                        chars.mid_right,
                        chars.horizontal,
                    ),
                )
                .with_rewrap_policy(WrappedLineRewrapPolicy::PreserveAndCrop),
            );
            continue;
        }

        rendered.push(
            rich_line(
                style,
                boxed_rich_table_row_chunks(row, &col_widths, chars.vertical),
            )
            .with_rewrap_policy(WrappedLineRewrapPolicy::PreserveAndCrop),
        );
    }

    rendered.push(
        rich_line(
            style,
            border_chunks(
                &col_widths,
                chars.bottom_left,
                chars.bottom_mid,
                chars.bottom_right,
                chars.horizontal,
            ),
        )
        .with_rewrap_policy(WrappedLineRewrapPolicy::PreserveAndCrop),
    );
    rendered
}

fn compute_rich_table_widths(rows: &[RichTableRow], width: usize) -> Vec<usize> {
    let col_count = rows.iter().map(|row| row.cells.len()).max().unwrap_or(0);
    let mut col_widths = vec![3usize; col_count];
    for row in rows {
        if row.is_separator {
            continue;
        }
        for (index, cell) in row.cells.iter().enumerate() {
            col_widths[index] = col_widths[index].max(inline_chunks_width(cell).min(40));
        }
    }

    let min_widths = vec![3usize; col_count];
    let separator_width = col_count * 3 + 1;
    let max_budget = width.saturating_sub(separator_width);
    while col_widths.iter().sum::<usize>() > max_budget {
        let Some((index, _)) = col_widths
            .iter()
            .enumerate()
            .filter(|(index, value)| **value > min_widths[*index])
            .max_by_key(|(_, value)| **value)
        else {
            break;
        };
        col_widths[index] = col_widths[index].saturating_sub(1);
    }
    col_widths
}

fn border_chunks(
    col_widths: &[usize],
    left: &str,
    middle: &str,
    right: &str,
    horizontal: &str,
) -> Vec<InlineChunk> {
    let mut chunks = vec![InlineChunk::styled(
        WrappedSpanStyle::TableBorder,
        left.to_string(),
    )];
    for (index, width) in col_widths.iter().enumerate() {
        chunks.push(InlineChunk::styled(
            WrappedSpanStyle::TableBorder,
            horizontal.repeat(width.saturating_add(2)),
        ));
        chunks.push(InlineChunk::styled(
            WrappedSpanStyle::TableBorder,
            if index + 1 == col_widths.len() {
                right.to_string()
            } else {
                middle.to_string()
            },
        ));
    }
    chunks
}

fn boxed_rich_table_row_chunks(
    row: &RichTableRow,
    col_widths: &[usize],
    vertical: &str,
) -> Vec<InlineChunk> {
    let mut chunks = vec![InlineChunk::styled(
        WrappedSpanStyle::TableBorder,
        vertical.to_string(),
    )];
    for (index, width) in col_widths.iter().enumerate() {
        let cell = row.cells.get(index).cloned().unwrap_or_default();
        chunks.push(InlineChunk::plain(" "));
        chunks.extend(crop_inline_chunks_to_width(
            &cell,
            *width,
            if row.is_header {
                Some(WrappedSpanStyle::TableHeader)
            } else {
                None
            },
        ));
        let used = inline_chunks_width(&crop_inline_chunks_to_width(
            &cell,
            *width,
            if row.is_header {
                Some(WrappedSpanStyle::TableHeader)
            } else {
                None
            },
        ));
        if used < *width {
            chunks.push(InlineChunk::plain(" ".repeat(*width - used)));
        }
        chunks.push(InlineChunk::plain(" "));
        chunks.push(InlineChunk::styled(
            WrappedSpanStyle::TableBorder,
            vertical.to_string(),
        ));
    }
    chunks
}

fn crop_inline_chunks_to_width(
    chunks: &[InlineChunk],
    width: usize,
    force_style: Option<WrappedSpanStyle>,
) -> Vec<InlineChunk> {
    if width == 0 {
        return Vec::new();
    }

    let text_width = inline_chunks_width(chunks);
    let budget = if text_width > width && width > 1 {
        width - 1
    } else {
        width
    };
    let mut output = Vec::new();
    let mut used = 0usize;

    for chunk in chunks {
        let style = force_style.or(chunk.style());
        let mut current = String::new();
        for ch in chunk.text().chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
            if used + ch_width > budget {
                break;
            }
            current.push(ch);
            used += ch_width;
        }
        if !current.is_empty() {
            push_chunk(
                &mut output,
                match style {
                    Some(style) => InlineChunk::styled(style, current),
                    None => InlineChunk::plain(current),
                },
            );
        }
        if used >= budget {
            break;
        }
    }

    if text_width > width {
        if let Some(last) = output.last_mut() {
            last.append_text("…");
        } else {
            output.push(match force_style {
                Some(style) => InlineChunk::styled(style, "…"),
                None => InlineChunk::plain("…"),
            });
        }
    }

    output
}

fn inline_chunks_width(chunks: &[InlineChunk]) -> usize {
    chunks.iter().map(|chunk| display_width(chunk.text())).sum()
}

fn table_chars(capabilities: TerminalCapabilities) -> TableChars<'static> {
    if capabilities.ascii_only() {
        TableChars {
            top_left: "+",
            top_mid: "+",
            top_right: "+",
            mid_left: "+",
            mid_mid: "+",
            mid_right: "+",
            bottom_left: "+",
            bottom_mid: "+",
            bottom_right: "+",
            horizontal: "-",
            vertical: "|",
        }
    } else {
        TableChars {
            top_left: "┌",
            top_mid: "┬",
            top_right: "┐",
            mid_left: "├",
            mid_mid: "┼",
            mid_right: "┤",
            bottom_left: "└",
            bottom_mid: "┴",
            bottom_right: "┘",
            horizontal: "─",
            vertical: "│",
        }
    }
}

fn wrap_inline_chunks(
    chunks: Vec<InlineChunk>,
    width: usize,
    capabilities: TerminalCapabilities,
) -> Vec<Vec<InlineChunk>> {
    wrap_inline_chunks_with_widths(chunks, width, width, capabilities)
}

fn wrap_inline_chunks_with_widths(
    chunks: Vec<InlineChunk>,
    first_width: usize,
    subsequent_width: usize,
    capabilities: TerminalCapabilities,
) -> Vec<Vec<InlineChunk>> {
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;
    let mut current_limit = first_width.max(1);
    let mut pending_space = false;

    for token in tokenize_inline_chunks(chunks) {
        match token {
            InlineToken::Whitespace => pending_space = true,
            InlineToken::Chunk(chunk) => {
                for piece in split_inline_chunk_to_width(&chunk, current_limit.max(1), capabilities)
                {
                    let piece_width = display_width(piece.text());
                    let space_width = usize::from(pending_space && current_width > 0);
                    if current_width > 0
                        && current_width + space_width + piece_width > current_limit
                    {
                        lines.push(current);
                        current = Vec::new();
                        current_width = 0;
                        current_limit = subsequent_width.max(1);
                        pending_space = false;
                    }

                    if pending_space && current_width > 0 {
                        current.push(InlineChunk::plain(" "));
                        current_width += 1;
                    }
                    pending_space = false;

                    current_width += piece_width;
                    current.push(piece);
                }
            },
        }
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

enum InlineToken {
    Whitespace,
    Chunk(InlineChunk),
}

fn tokenize_inline_chunks(chunks: Vec<InlineChunk>) -> Vec<InlineToken> {
    let mut tokens = Vec::new();
    for chunk in chunks {
        if matches!(
            chunk.style(),
            Some(WrappedSpanStyle::InlineCode | WrappedSpanStyle::Link)
        ) {
            tokens.push(InlineToken::Chunk(chunk));
            continue;
        }

        let mut current = String::new();
        let mut whitespace = None;
        for ch in chunk.text().chars() {
            let is_whitespace = ch.is_whitespace();
            match whitespace {
                Some(flag) if flag == is_whitespace => current.push(ch),
                Some(flag) => {
                    if flag {
                        tokens.push(InlineToken::Whitespace);
                    } else {
                        tokens.push(InlineToken::Chunk(match chunk.style() {
                            Some(style) => InlineChunk::styled(style, current.clone()),
                            None => InlineChunk::plain(current.clone()),
                        }));
                    }
                    current.clear();
                    current.push(ch);
                    whitespace = Some(is_whitespace);
                },
                None => {
                    current.push(ch);
                    whitespace = Some(is_whitespace);
                },
            }
        }
        if !current.is_empty() {
            if whitespace == Some(true) {
                tokens.push(InlineToken::Whitespace);
            } else {
                tokens.push(InlineToken::Chunk(match chunk.style() {
                    Some(style) => InlineChunk::styled(style, current),
                    None => InlineChunk::plain(current),
                }));
            }
        }
    }
    tokens
}

fn split_inline_chunk_to_width(
    chunk: &InlineChunk,
    width: usize,
    capabilities: TerminalCapabilities,
) -> Vec<InlineChunk> {
    split_token_by_width(chunk.text(), width.max(1), capabilities)
        .into_iter()
        .map(|piece| match chunk.style() {
            Some(style) => InlineChunk::styled(style, piece.into_owned()),
            None => InlineChunk::plain(piece.into_owned()),
        })
        .collect()
}

fn flatten_inline_chunks(chunks: Vec<InlineChunk>) -> String {
    chunks
        .into_iter()
        .map(|chunk| chunk.text().to_string())
        .collect::<String>()
}

fn split_token_by_width<'a>(
    token: &'a str,
    width: usize,
    _capabilities: TerminalCapabilities,
) -> Vec<Cow<'a, str>> {
    let width = width.max(1);
    if display_width(token) <= width {
        return vec![Cow::Borrowed(token)];
    }
    split_preserving_width(token, width)
}

fn split_preserving_width<'a>(text: &'a str, width: usize) -> Vec<Cow<'a, str>> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if current_width + ch_width > width && !current.is_empty() {
            chunks.push(Cow::Owned(current));
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }

    if !current.is_empty() {
        chunks.push(Cow::Owned(current));
    }
    chunks
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::{flatten_inline_text, render_literal_text, render_markdown_lines, wrap_text};
    use crate::{
        capability::{ColorLevel, GlyphMode, TerminalCapabilities},
        state::{WrappedLineStyle, WrappedSpanStyle},
    };

    fn unicode_capabilities() -> TerminalCapabilities {
        TerminalCapabilities {
            color: ColorLevel::TrueColor,
            glyphs: GlyphMode::Unicode,
            alt_screen: false,
            mouse: false,
            bracketed_paste: false,
        }
    }

    #[test]
    fn wrap_text_preserves_hanging_indent_for_lists() {
        let lines = wrap_text(
            "- 第一项需要被正确换行，并且后续行要和正文对齐",
            18,
            unicode_capabilities(),
        );
        assert!(lines[0].starts_with("- "));
        assert!(lines[1].starts_with("  "));
    }

    #[test]
    fn wrap_text_formats_markdown_tables() {
        let lines = wrap_text(
            "| 工具 | 说明 |\n| --- | --- |\n| **reviewnow** | 代码审查 |\n| `git-commit` | \
             自动提交 |",
            32,
            unicode_capabilities(),
        );
        assert!(lines.iter().any(|line| line.contains("┌")));
        assert!(lines.iter().any(|line| line.contains("│ 工具")));
        assert!(lines.iter().any(|line| line.contains("reviewnow")));
        assert!(lines.iter().any(|line| line.contains("git-commit")));
        assert!(lines.iter().all(|line| !line.contains("**reviewnow**")));
        assert!(lines.iter().all(|line| !line.contains("`git-commit`")));
    }

    #[test]
    fn inline_markdown_keeps_emphasis_body_before_cjk_punctuation() {
        assert_eq!(flatten_inline_text("**writeFile**。"), "writeFile。");
    }

    #[test]
    fn wrap_literal_text_preserves_user_markdown_markers() {
        let lines = render_literal_text(
            "## 用户原文\n请保留 **readFile** 和 [link](https://example.com)。",
            120,
            unicode_capabilities(),
        );
        let joined = lines.join("\n");
        assert!(joined.contains("## 用户原文"));
        assert!(joined.contains("**readFile**"));
        assert!(joined.contains("[link](https://example.com)"));
    }

    #[test]
    fn parser_marks_rich_span_styles() {
        let lines = render_markdown_lines(
            "## 标题\n\n使用 `writeFile`\n\n- [readFile](https://example.com)\n> \
             引用\n\n```rs\nlet x = 1;\n```\n\n| 工具 | 说明 |\n| --- | --- |\n| writeFile | 保存 \
             |",
            64,
            unicode_capabilities(),
            WrappedLineStyle::Plain,
        );
        let span_styles = lines
            .iter()
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style))
            .collect::<Vec<_>>();
        assert!(span_styles.contains(&WrappedSpanStyle::Heading));
        assert!(span_styles.contains(&WrappedSpanStyle::Link));
        assert!(span_styles.contains(&WrappedSpanStyle::ListMarker));
        assert!(span_styles.contains(&WrappedSpanStyle::QuoteMarker));
        assert!(span_styles.contains(&WrappedSpanStyle::CodeText));
        assert!(span_styles.contains(&WrappedSpanStyle::TableHeader));
        assert!(span_styles.contains(&WrappedSpanStyle::InlineCode));
    }

    #[test]
    fn fenced_ascii_tree_block_preserves_structure() {
        let lines = wrap_text(
            "```\nfrontend/src/\n├─ components/\n│ ├─ Chat/\n│ └─ Sidebar/\n└─ store/\n```",
            18,
            unicode_capabilities(),
        );
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "frontend/src/");
        assert!(lines.iter().any(|line| line.contains("├─")));
        assert!(lines.iter().any(|line| line.contains("│")));
    }

    #[test]
    fn unfenced_ascii_diagram_uses_preformatted_fallback() {
        let lines = wrap_text(
            "┌──────────────┐\n│ server       │\n├──────┬───────┤\n│ core │ cli   \
             │\n└──────┴───────┘",
            18,
            unicode_capabilities(),
        );
        assert_eq!(lines.len(), 5);
        assert!(lines.iter().all(|line| !line.contains("…")));
        assert!(lines.iter().any(|line| line.contains("┌")));
        assert!(lines.iter().any(|line| line.contains("┴")));
    }

    #[test]
    fn fenced_ascii_diagram_uses_text_art_without_fence_markers() {
        let lines = render_markdown_lines(
            "```\n    ┌──────────┐\n    │ diagram │\n    └──────────┘\n```",
            18,
            unicode_capabilities(),
            WrappedLineStyle::Plain,
        );
        let joined = lines.iter().map(|line| line.text()).collect::<Vec<_>>();
        let span_styles = lines
            .iter()
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style))
            .collect::<Vec<_>>();

        assert!(joined.iter().all(|line| !line.contains("```")));
        assert!(joined.iter().any(|line| line.contains("┌")));
        assert!(span_styles.contains(&WrappedSpanStyle::TextArt));
    }

    #[test]
    fn pseudo_fence_ascii_diagram_drops_fence_lines() {
        let lines = wrap_text(
            "``\n┌──────┐\n│ test │\n└──────┘\n``",
            18,
            unicode_capabilities(),
        );
        assert_eq!(lines, vec!["┌──────┐", "│ test │", "└──────┘"]);
    }
}
