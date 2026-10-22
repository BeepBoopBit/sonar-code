//! Oxc supplies byte-accurate declarations for both TypeScript dialects.
use anyhow::Result;
use oxc_allocator::Allocator;
use oxc_ast::{AstKind, ast::*};
use oxc_ast_visit::Visit;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};

use crate::extract::{ParsedFile, ParsedLocation, regions};

pub(super) fn parse(path: &str, source: &str) -> Result<ParsedFile> {
    let allocator = Allocator::default();
    let result = Parser::new(&allocator, source, SourceType::from_path(path)?).parse();
    let mut parsed = crate::extract::resource(source);
    parsed.has_errors = !result.errors.is_empty();
    parsed.locations[0].is_test = super::test_file(path);
    let comments: Vec<_> = result.program.comments.iter().map(|c| c.span).collect();
    let mut end = 0;
    for comment in &comments {
        if !source[end..comment.start as usize].trim().is_empty() {
            break;
        }
        end = comment.end as usize;
    }
    parsed.locations[0].docs = source[..end].to_owned();
    let mut collector = Collector {
        source,
        comments,
        lines: std::iter::once(0)
            .chain(
                source
                    .bytes()
                    .enumerate()
                    .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
            )
            .collect(),
        locations: parsed.locations,
        bodies: vec![None],
        nodes: Vec::new(),
        parents: Vec::new(),
        parent: 0,
    };
    collector.visit_program(&result.program);
    collector.merge_overloads();
    parsed.locations = collector.locations;
    Ok(parsed)
}

struct Declaration {
    kind: &'static str,
    span: Span,
    name: Span,
    body: Option<usize>,
    prefix: u32,
}

struct Collector<'a> {
    source: &'a str,
    comments: Vec<Span>,
    lines: Vec<usize>,
    locations: Vec<ParsedLocation>,
    /// Body starts distinguish implemented functions from their overload signatures.
    bodies: Vec<Option<usize>>,
    nodes: Vec<AstKind<'a>>,
    parents: Vec<usize>,
    parent: usize,
}

fn value_declaration(value: &Expression<'_>) -> Option<(&'static str, usize)> {
    Some(match value.get_inner_expression() {
        Expression::ArrowFunctionExpression(f) => ("function", f.body.span.start as usize),
        Expression::FunctionExpression(f) => ("function", f.body.as_ref()?.span.start as usize),
        Expression::ClassExpression(c) => ("class", c.body.span.start as usize),
        _ => return None,
    })
}

fn literal_key(key: &PropertyKey<'_>, computed: bool) -> Option<Span> {
    (!computed
        && matches!(
            key,
            PropertyKey::StaticIdentifier(_)
                | PropertyKey::PrivateIdentifier(_)
                | PropertyKey::StringLiteral(_)
                | PropertyKey::NumericLiteral(_)
        ))
    .then(|| key.span())
}

impl Collector<'_> {
    fn line(&self, byte: usize) -> usize {
        self.lines.partition_point(|start| *start <= byte)
    }

    fn module_binding(&self) -> bool {
        for node in self.nodes.iter().rev() {
            match node {
                AstKind::Program(_) | AstKind::TSModuleDeclaration(_) => return true,
                AstKind::Function(_) | AstKind::ArrowFunctionExpression(_) | AstKind::Class(_) => {
                    return false;
                }
                _ => {}
            }
        }
        false
    }

    fn binding_name(&self, name: &BindingIdentifier<'_>) -> Span {
        // Oxc extends a binding identifier's span through its type annotation.
        // Retain the literal identifier token, including any Unicode escapes.
        let raw = &self.source[name.span.start as usize..name.span.end as usize];
        let width = raw
            .find(|c: char| c.is_whitespace() || matches!(c, ':' | '?' | '!' | '/'))
            .unwrap_or(raw.len());
        Span::new(name.span.start, name.span.start + width as u32)
    }

    fn bound_value(&self, span: Span) -> bool {
        self.nodes.iter().rev().any(|n| match n {
            AstKind::VariableDeclarator(v) => v
                .init
                .as_ref()
                .is_some_and(|v| v.get_inner_expression().span() == span),
            AstKind::PropertyDefinition(p) => p
                .value
                .as_ref()
                .is_some_and(|v| v.get_inner_expression().span() == span),
            AstKind::ObjectProperty(p) => p.value.get_inner_expression().span() == span,
            AstKind::MethodDefinition(m) => m.value.span == span,
            _ => false,
        })
    }

    fn named_type_member(&self) -> bool {
        match self.nodes.last() {
            Some(AstKind::TSInterfaceDeclaration(_)) => true,
            Some(AstKind::TSTypeLiteral(_)) => matches!(
                self.nodes.iter().rev().nth(1),
                Some(AstKind::TSTypeAliasDeclaration(_))
            ),
            _ => false,
        }
    }

    fn declaration(&self, node: AstKind<'_>) -> Option<Declaration> {
        let (kind, name, body) = match node {
            AstKind::Function(f) if !self.bound_value(f.span) => (
                "function",
                f.id.as_ref()?.span,
                f.body.as_ref().map(|b| b.span.start as usize),
            ),
            AstKind::Class(c) if !self.bound_value(c.span) => (
                "class",
                c.id.as_ref()?.span,
                Some(c.body.span.start as usize),
            ),
            AstKind::VariableDeclarator(v) => {
                let BindingPatternKind::BindingIdentifier(name) = &v.id.kind else {
                    return None;
                };
                let (kind, body) =
                    if let Some((kind, body)) = v.init.as_ref().and_then(value_declaration) {
                        (kind, Some(body))
                    } else if self.module_binding() {
                        (
                            if v.kind.is_const() {
                                "constant"
                            } else {
                                "variable"
                            },
                            None,
                        )
                    } else {
                        return None;
                    };
                (kind, self.binding_name(name), body)
            }
            AstKind::MethodDefinition(m) => (
                "function",
                literal_key(&m.key, m.computed)?,
                m.value.body.as_ref().map(|b| b.span.start as usize),
            ),
            AstKind::PropertyDefinition(p) => {
                let (kind, body) = p
                    .value
                    .as_ref()
                    .and_then(value_declaration)
                    .filter(|(k, _)| *k == "function")
                    .map_or(("field", None), |(k, b)| (k, Some(b)));
                (kind, literal_key(&p.key, p.computed)?, body)
            }
            AstKind::AccessorProperty(p) => ("field", literal_key(&p.key, p.computed)?, None),
            AstKind::ObjectProperty(p) => {
                let (kind, body) = value_declaration(&p.value).filter(|(k, _)| *k == "function")?;
                (kind, literal_key(&p.key, p.computed)?, Some(body))
            }
            AstKind::TSInterfaceDeclaration(i) => {
                ("interface", i.id.span, Some(i.body.span.start as usize))
            }
            AstKind::TSTypeAliasDeclaration(t) => ("type", t.id.span, None),
            AstKind::TSPropertySignature(p) if self.named_type_member() => {
                ("field", literal_key(&p.key, p.computed)?, None)
            }
            AstKind::TSMethodSignature(m) if self.named_type_member() => {
                ("function", literal_key(&m.key, m.computed)?, None)
            }
            AstKind::TSEnumDeclaration(e) => ("enum", e.id.span, Some(e.body.span.start as usize)),
            AstKind::TSEnumMember(m) => ("variant", m.id.span(), None),
            AstKind::TSModuleDeclaration(m) => (
                "module",
                m.id.span(),
                m.body.as_ref().map(|b| b.span().start as usize),
            ),
            _ => return None,
        };
        let prefix = match node {
            AstKind::Class(c) => c.decorators.first().map(|d| d.span.start),
            AstKind::MethodDefinition(m) => m.decorators.first().map(|d| d.span.start),
            AstKind::PropertyDefinition(p) => p.decorators.first().map(|d| d.span.start),
            _ => None,
        }
        .unwrap_or(node.span().start);
        Some(Declaration {
            kind,
            span: node.span(),
            name,
            body,
            prefix,
        })
    }

    fn range(&self, mut span: Span) -> Span {
        for node in self.nodes.iter().rev() {
            match node {
                AstKind::VariableDeclaration(v) if v.declarations.len() == 1 => span = v.span,
                AstKind::ExportNamedDeclaration(e) => span = e.span,
                AstKind::ExportDefaultDeclaration(e) => span = e.span,
                _ => break,
            }
        }
        span
    }

    fn docs(&self, start: usize) -> (String, usize) {
        let mut boundary = start;
        let mut first = start;
        let mut comments = Vec::new();
        for comment in self.comments[..self.comments.partition_point(|c| c.end as usize <= start)]
            .iter()
            .rev()
        {
            if !self.source[comment.end as usize..boundary]
                .trim()
                .is_empty()
            {
                break;
            }
            comments.push(&self.source[comment.start as usize..comment.end as usize]);
            first = comment.start as usize;
            boundary = first;
        }
        comments.reverse();
        (comments.join("\n"), self.line(first))
    }

    fn add(&mut self, declaration: Declaration) {
        let mut range = self.range(declaration.span);
        range.start = range.start.min(declaration.prefix);
        let bare = &self.source[declaration.name.start as usize..declaration.name.end as usize];
        let scope = self.locations[self.parent].symbol.as_deref().unwrap_or("");
        let symbol = if scope.is_empty() {
            bare.to_owned()
        } else {
            format!("{scope}.{bare}")
        };
        let start = range.start as usize;
        let end = range.end as usize;
        let signature_end = declaration.body.unwrap_or(end);
        let (docs, docs_start) = self.docs(start);
        let region_end = if matches!(declaration.kind, "class" | "interface" | "enum" | "module") {
            signature_end
        } else {
            end
        };
        let index = self.locations.len();
        self.locations.push(ParsedLocation {
            symbol: Some(symbol),
            kind: declaration.kind,
            definition: Some(self.line(declaration.span.start as usize)),
            parent: Some(self.parent),
            docs,
            docs_start,
            signature: self.source[start..signature_end].trim_end().to_owned(),
            start_byte: start,
            end_byte: end,
            name_start: declaration.name.start as usize,
            name_end: declaration.name.end as usize,
            is_test: self.locations[self.parent].is_test,
            regions: regions(&self.source[start..region_end], self.line(start)),
        });
        self.bodies.push(declaration.body);
        self.parent = index;
    }

    fn only_comments(&self, start: usize, end: usize) -> bool {
        let mut boundary = start;
        for c in &self.comments[self
            .comments
            .partition_point(|c| (c.start as usize) < start)..]
        {
            if c.end as usize > end {
                break;
            }
            if !self.source[boundary..c.start as usize].trim().is_empty() {
                return false;
            }
            boundary = c.end as usize;
        }
        self.source[boundary..end].trim().is_empty()
    }

    fn merge_overloads(&mut self) {
        let mut removed = std::collections::BTreeSet::new();
        for index in 1..self.locations.len() {
            if self.locations[index].kind != "function" || self.bodies[index].is_none() {
                continue;
            }
            let mut first = index;
            while first > 1 {
                let before = &self.locations[first - 1];
                let owner = &self.locations[index];
                if before.kind != "function"
                    || self.bodies[first - 1].is_some()
                    || before.symbol != owner.symbol
                    || before.parent != owner.parent
                    || !self.only_comments(before.end_byte, self.locations[first].start_byte)
                {
                    break;
                }
                first -= 1;
                removed.insert(first);
            }
            if first != index {
                let start = self.locations[first].start_byte;
                let (docs, docs_start) = self.docs(start);
                let line = self.line(start);
                let owner = &mut self.locations[index];
                owner.start_byte = start;
                owner.signature = self.source[start..self.bodies[index].unwrap()]
                    .trim_end()
                    .to_owned();
                owner.regions = regions(&self.source[start..owner.end_byte], line);
                if owner.docs.is_empty() {
                    owner.docs = docs;
                    owner.docs_start = docs_start;
                }
            }
        }
        let mut mapping = Vec::with_capacity(self.locations.len());
        let mut next = 0;
        for i in 0..self.locations.len() {
            mapping.push(next);
            if !removed.contains(&i) {
                next += 1;
            }
        }
        self.locations = std::mem::take(&mut self.locations)
            .into_iter()
            .enumerate()
            .filter_map(|(i, mut l)| {
                if removed.contains(&i) {
                    return None;
                }
                l.parent = l.parent.map(|p| mapping[p]);
                Some(l)
            })
            .collect();
    }
}

impl<'a> Visit<'a> for Collector<'a> {
    fn enter_node(&mut self, node: AstKind<'a>) {
        self.parents.push(self.parent);
        if let Some(declaration) = self.declaration(node) {
            self.add(declaration);
        }
        self.nodes.push(node);
    }
    fn leave_node(&mut self, _node: AstKind<'a>) {
        self.nodes.pop();
        self.parent = self.parents.pop().unwrap();
    }
}
