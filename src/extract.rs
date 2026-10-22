pub(crate) struct ParsedFile {
    pub locations: Vec<ParsedLocation>,
    pub has_errors: bool,
}

pub(crate) struct ParsedLocation {
    pub symbol: Option<String>,
    pub kind: &'static str,
    pub definition: Option<usize>,
    pub parent: Option<usize>,
    pub docs: String,
    pub docs_start: usize,
    pub signature: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub name_start: usize,
    pub name_end: usize,
    pub is_test: bool,
    pub regions: Vec<Region>,
}

pub(crate) struct Region {
    pub start: usize,
    pub end: usize,
    pub source: String,
}

/// Bounded plain-text resource evidence. No YAML nodes, values or references are inferred.
pub(crate) fn resource(source: &str) -> ParsedFile {
    ParsedFile {
        has_errors: false,
        locations: vec![ParsedLocation {
            symbol: None,
            kind: "file",
            definition: None,
            parent: None,
            docs: String::new(),
            docs_start: 1,
            signature: String::new(),
            start_byte: 0,
            end_byte: source.len(),
            name_start: 0,
            name_end: 0,
            is_test: false,
            regions: regions(source, 1),
        }],
    }
}

/// Markdown headings delimit independent navigational sections. Code fences cannot create headings.
pub(crate) fn markdown(source: &str) -> ParsedFile {
    let lines: Vec<_> = source.lines().collect();
    let mut boundaries = vec![0];
    let mut fence: Option<(char, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let width = trimmed.chars().take_while(|c| *c == marker).count();
        if matches!(marker, '`' | '~') && width >= 3 {
            match fence {
                None => fence = Some((marker, width)),
                Some((open, count)) if open == marker && width >= count => fence = None,
                _ => {}
            }
            continue;
        }
        if fence.is_none()
            && marker == '#'
            && width <= 6
            && trimmed.as_bytes().get(width) == Some(&b' ')
            && i > 0
        {
            boundaries.push(i);
        }
    }
    boundaries.push(lines.len());
    let locations = boundaries
        .windows(2)
        .filter(|r| r[0] < r[1])
        .map(|r| {
            let body = lines[r[0]..r[1]].join("\n");
            ParsedLocation {
                symbol: None,
                kind: "documentation",
                definition: None,
                parent: None,
                docs: body.clone(),
                docs_start: r[0] + 1,
                signature: lines[r[0]].to_owned(),
                start_byte: 0,
                end_byte: 0,
                name_start: 0,
                name_end: 0,
                is_test: false,
                regions: regions(&body, r[0] + 1),
            }
        })
        .collect();
    ParsedFile {
        locations,
        has_errors: false,
    }
}

pub(crate) fn regions(source: &str, start: usize) -> Vec<Region> {
    const WIDTH: usize = 32;
    const STRIDE: usize = 24;
    let lines: Vec<_> = source.lines().collect();
    if lines.is_empty() {
        return vec![Region {
            start,
            end: start,
            source: String::new(),
        }];
    }
    let mut result = Vec::new();
    for offset in (0..lines.len()).step_by(STRIDE) {
        let end = (offset + WIDTH).min(lines.len());
        result.push(Region {
            start: start + offset,
            end: start + end - 1,
            source: lines[offset..end].join("\n"),
        });
        if end == lines.len() {
            break;
        }
    }
    result
}

pub(crate) fn description_regions(source: &str, start: usize) -> Vec<Region> {
    regions(source, start)
}
