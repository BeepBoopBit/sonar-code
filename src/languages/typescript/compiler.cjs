// Explicit, offline analysis with the official compiler. Repository code is never loaded.
const fs = require('node:fs')
const path = require('node:path')
const crypto = require('node:crypto')
const {createRequire} = require('node:module')
const input = JSON.parse(fs.readFileSync(0, 'utf8'))
const requireFromRoot = createRequire(path.join(input.root, 'package.json'))
const compilerPath = input.compiler ? require.resolve(path.resolve(input.compiler)) : requireFromRoot.resolve('typescript')
const ts = require(compilerPath)
if (!ts.version || Number(ts.version.split('.')[0]) < 5 || Number(ts.version.split('.')[0]) >= 7) {
  throw new Error('TypeScript analysis requires the TypeScript 5.x or 6.x compiler API')
}
const inputs = new Map()
const absentDirectories = new Set()
const directoryListings = new Map()
const fileExists = (filename) => {
  const exists = ts.sys.fileExists(filename)
  if (!exists) inputs.set(path.resolve(filename), null)
  return exists
}
const directoryExists = (filename) => {
  const exists = ts.sys.directoryExists(filename)
  if (!exists) absentDirectories.add(path.resolve(filename))
  return exists
}
const getDirectories = (filename) => {
  const names = ts.sys.getDirectories(filename)
  if (directoryExists(filename)) directoryListings.set(path.resolve(filename), [...names].sort())
  return names
}
let indexed = new Map()
const digest = (text) => crypto.createHash('sha256').update(text).digest('hex')
const read = (filename) => {
  const value = ts.sys.readFile(filename)
  inputs.set(path.resolve(filename), value === undefined ? null : digest(fs.readFileSync(filename)))
  return indexed.get(path.resolve(filename))?.source ?? value
}
read(compilerPath)
indexed = new Map(input.files.map((file) => [path.resolve(input.root, file.path), file]))
const configs = new Map()
const configReads = new Set()
const configHost = {...ts.sys, readFile: read, fileExists, directoryExists, getDirectories}
function project(filename) {
  filename = path.resolve(filename)
  if (configReads.has(filename)) return
  configReads.add(filename)
  const json = ts.readConfigFile(filename, read)
  if (json.error) throw new Error(ts.flattenDiagnosticMessageText(json.error.messageText, '\n'))
  const config = ts.parseJsonConfigFileContent(json.config, configHost, path.dirname(filename), undefined, filename)
  const errors = config.errors.filter((d) => d.code !== 18003)
  if (errors.length) throw new Error(errors.map((d) => ts.flattenDiagnosticMessageText(d.messageText, '\n')).join('\n'))
  const roots = config.fileNames.filter((file) => indexed.has(path.resolve(file)))
  if (roots.length) configs.set(filename, {roots, options: {...config.options, noEmit: true, disableSourceOfProjectReferenceRedirect: false}})
  for (const reference of config.projectReferences || []) project(ts.resolveProjectReferencePath(reference))
}
const configPath = input.project || ts.findConfigFile(input.root, fileExists)
if (configPath) project(configPath)
const configured = new Set([...configs.values()].flatMap((c) => c.roots.map((r) => path.resolve(r))))
const uncovered = [...indexed.keys()].filter((p) => !configured.has(p))
if (uncovered.length) configs.set('<inferred>', {roots: uncovered, options: {
  target: ts.ScriptTarget.ESNext, module: ts.ModuleKind.ESNext,
  moduleResolution: ts.ModuleResolutionKind.Bundler, jsx: ts.JsxEmit.Preserve,
  strict: true, noEmit: true, allowImportingTsExtensions: true, skipLibCheck: true,
}})
const locations = new Map()
const locationKinds = new Map(input.locations.map((l) => [l.id, l.kind]))
for (const l of input.locations) {
  const filename = path.resolve(input.root, l.path)
  if (!locations.has(filename)) locations.set(filename, [])
  locations.get(filename).push(l)
}
const bytes = new Map()
function offset(file, position) {
  let mapping = bytes.get(file.fileName)
  if (!mapping) {
    mapping = new Uint32Array(file.text.length + 1)
    let byte = 0
    for (let i = 0; i < file.text.length;) {
      const cp = file.text.codePointAt(i), width = cp > 0xffff ? 2 : 1
      mapping[i] = byte
      if (width === 2) mapping[i + 1] = byte
      byte += Buffer.byteLength(String.fromCodePoint(cp))
      i += width
      mapping[i] = byte
    }
    bytes.set(file.fileName, mapping)
  }
  return mapping[position]
}
function token(file, node) {
  return [offset(file, node.getStart(file)), offset(file, node.end)]
}
function declarationName(declaration) {
  if (ts.isArrowFunction(declaration) || ts.isFunctionExpression(declaration) || ts.isClassExpression(declaration)) {
    let parent = declaration.parent
    while (parent && (ts.isParenthesizedExpression(parent) || ts.isAsExpression(parent) ||
        ts.isSatisfiesExpression(parent) || ts.isNonNullExpression(parent))) parent = parent.parent
    if (parent && (ts.isVariableDeclaration(parent) || ts.isPropertyDeclaration(parent) || ts.isPropertyAssignment(parent))) return parent.name
    if (ts.isFunctionExpression(declaration) && parent && ts.isMethodDeclaration(parent)) return parent.name
  }
  if (ts.isConstructorDeclaration(declaration)) return declaration.getChildren().find((n) => n.kind === ts.SyntaxKind.ConstructorKeyword)
  return declaration.name
}
function mapped(declaration) {
  if (!declaration) return undefined
  const file = declaration.getSourceFile(), locs = locations.get(path.resolve(file.fileName))
  if (ts.isSourceFile(declaration)) return locs?.find((l) => l.kind === 'file')
  const name = declarationName(declaration)
  if (!locs || !name || ts.isComputedPropertyName(name) || ts.isBindingPattern(name)) return undefined
  const [start, end] = token(file, name)
  const exact = locs.filter((l) => l.name_start === start && l.name_end === end)
  if (exact.length === 1) return exact[0]
  // Collapsed overload names must lie inside the implementation's recorded extent.
  const contained = locs.filter((l) => l.kind === 'function' && l.start <= start && l.end >= end &&
    Buffer.from(file.text).subarray(l.name_start, l.name_end).toString() === name.getText(file))
  return contained.length === 1 ? contained[0] : undefined
}
const candidates = new Map(), fields = new Map(), diagnostics = [], visited = new Set()
let skipped = 0, ambiguous = 0, definitions = new Set(), diagnosticCount = 0
const ownerAt = (locs, start, end) => locs.filter((l) => l.start <= start && l.end >= end)
  .sort((a, b) => (a.end - a.start) - (b.end - b.start) || a.id - b.id)[0]
function syntacticContext(file, node, owner) {
  for (let current = node; current; current = current.parent) {
    if (ts.isStatement(current) || ts.isVariableDeclaration(current)) {
      const [start, end] = token(file, current)
      const first = file.getLineAndCharacterOfPosition(current.getStart(file)).line
      const last = file.getLineAndCharacterOfPosition(current.end - 1).line
      if (start >= owner.start && end <= owner.end && end - start <= 4096 && last - first < 16) return [start, end]
      return undefined
    }
  }
}
function access(node) {
  if (ts.isBindingElement(node.parent)) return 'pattern'
  if (ts.isPropertyAssignment(node.parent) || ts.isShorthandPropertyAssignment(node.parent)) return 'initialize'
  let expression = node
  if (node.parent && (ts.isPropertyAccessExpression(node.parent) || ts.isElementAccessExpression(node.parent))) expression = node.parent
  let parent = expression.parent
  while (parent && ts.isParenthesizedExpression(parent)) { expression = parent; parent = parent.parent }
  if (parent && ts.isBinaryExpression(parent) && parent.left === expression &&
      parent.operatorToken.kind >= ts.SyntaxKind.FirstAssignment && parent.operatorToken.kind <= ts.SyntaxKind.LastAssignment) {
    return parent.operatorToken.kind === ts.SyntaxKind.EqualsToken ? 'write' : 'read_write'
  }
  if (parent && (ts.isPrefixUnaryExpression(parent) || ts.isPostfixUnaryExpression(parent)) &&
      [ts.SyntaxKind.PlusPlusToken, ts.SyntaxKind.MinusMinusToken].includes(parent.operator)) return 'read_write'
  if (parent && ts.isDeleteExpression(parent)) return 'write'
  if (parent && ts.isCallExpression(parent) && parent.expression === expression) return 'receiver'
  return 'read'
}
for (const [config, settings] of configs) {
  const host = ts.createCompilerHost(settings.options)
  host.readFile = read
  host.directoryExists = directoryExists
  host.fileExists = fileExists
  host.getDirectories = getDirectories
  const program = ts.createProgram({rootNames: settings.roots, options: settings.options, host})
  const checker = program.getTypeChecker()
  const errors = [...program.getSyntacticDiagnostics(), ...program.getSemanticDiagnostics()]
  diagnosticCount += errors.length
  for (const d of errors.slice(0, Math.max(0, 20 - diagnostics.length))) diagnostics.push({
    code: d.code, path: d.file ? path.relative(input.root, d.file.fileName).replaceAll(path.sep, '/') : null,
    line: d.file && d.start !== undefined ? d.file.getLineAndCharacterOfPosition(d.start).line + 1 : null,
    message: ts.flattenDiagnosticMessageText(d.messageText, '\n'),
  })
  function target(symbol) {
    if (!symbol) return undefined
    try { if (symbol.flags & ts.SymbolFlags.Alias) symbol = checker.getAliasedSymbol(symbol) } catch { return undefined }
    const ids = new Map()
    for (const d of symbol.declarations || []) { const l = mapped(d); if (l) ids.set(l.id, l) }
    if (ids.size > 1) { ambiguous++; return undefined }
    return ids.size === 1 ? [...ids.values()][0] : undefined
  }
  for (const file of program.getSourceFiles()) {
    const absolute = path.resolve(file.fileName), indexedFile = indexed.get(absolute), locs = locations.get(absolute)
    if (!indexedFile || !locs) continue
    visited.add(indexedFile.path)
    function record(node, resolved, kind) {
      if (!resolved) { skipped++; return }
      definitions.add(resolved.id)
      const [start, end] = token(file, node), owner = ownerAt(locs, start, end)
      if (!owner) { skipped++; return }
      const key = [indexedFile.path, start, end, kind].join(':')
      const edge = {from: owner.id, to: resolved.id, kind, path: indexedFile.path,
        line: file.getLineAndCharacterOfPosition(node.getStart(file)).line + 1, start, end,
        text: Buffer.from(file.text).subarray(start, end).toString()}
      if (!candidates.has(key)) candidates.set(key, new Map())
      candidates.get(key).set(resolved.id, edge)
      if (kind === 'references' && resolved.kind === 'field') fields.set(key, {
        ...edge, access: access(node), context: syntacticContext(file, node, owner),
      })
    }
    function visit(node) {
      if ((ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) && node.moduleSpecifier) {
        record(node.moduleSpecifier, target(checker.getSymbolAtLocation(node.moduleSpecifier)), 'references')
      }
      if (node.name || ts.isConstructorDeclaration(node)) {
        const declaration = mapped(node), name = declarationName(node)
        if (declaration && name && checker.getSymbolAtLocation(name)) definitions.add(declaration.id)
      }
      if (ts.isBindingElement(node) && ts.isObjectBindingPattern(node.parent) &&
          ts.isVariableDeclaration(node.parent.parent) && node.parent.parent.initializer) {
        const name = node.propertyName || node.name
        if (ts.isIdentifier(name) || ts.isStringLiteral(name)) {
          const symbol = checker.getTypeAtLocation(node.parent.parent.initializer).getProperty(name.text)
          const resolved = target(symbol)
          if (resolved?.kind === 'field') record(name, resolved, 'references')
        }
      }
      if ((ts.isPropertyAssignment(node) || ts.isShorthandPropertyAssignment(node)) && ts.isObjectLiteralExpression(node.parent)) {
        const name = node.name
        if (ts.isIdentifier(name) || ts.isStringLiteral(name)) {
          const resolved = target(checker.getContextualType(node.parent)?.getProperty(name.text))
          if (resolved?.kind === 'field') record(name, resolved, 'references')
        }
      }
      if (ts.isCallExpression(node) || ts.isNewExpression(node)) {
        let expression = node.expression
        while (ts.isParenthesizedExpression(expression) || ts.isNonNullExpression(expression)) expression = expression.expression
        const name = ts.isPropertyAccessExpression(expression) ? expression.name :
          ts.isElementAccessExpression(expression) ? expression.argumentExpression : expression
        let resolved
        try { resolved = mapped(checker.getResolvedSignature(node)?.declaration) } catch {}
        if (!resolved) resolved = target(checker.getSymbolAtLocation(name))
        if (resolved?.kind === 'function') record(name, resolved, 'calls')
      }
      if ((ts.isIdentifier(node) || ts.isPrivateIdentifier(node) ||
          (ts.isStringLiteral(node) && ts.isElementAccessExpression(node.parent))) &&
          node.parent.name !== node) {
        const symbol = ts.isShorthandPropertyAssignment(node.parent) ? checker.getShorthandAssignmentValueSymbol(node.parent) : checker.getSymbolAtLocation(node)
        record(node, target(symbol), 'references')
      } else if ((ts.isIdentifier(node) || ts.isPrivateIdentifier(node)) && ts.isPropertyAccessExpression(node.parent)) {
        record(node, target(checker.getSymbolAtLocation(node)), 'references')
      }
      ts.forEachChild(node, visit)
    }
    visit(file)
  }
}
const edges = []
for (const resolutions of candidates.values()) {
  if (resolutions.size === 1) edges.push([...resolutions.values()][0])
  else ambiguous++
}
const fieldUses = []
const edgesByOwner = new Map()
for (const edge of edges) {
  const key = edge.path + ':' + edge.from
  if (!edgesByOwner.has(key)) edgesByOwner.set(key, [])
  edgesByOwner.get(key).push(edge)
}
for (const [key, use] of fields) {
  if (candidates.get(key)?.size !== 1) continue
  use.associations = use.context ? (edgesByOwner.get(use.path + ':' + use.from) || []).filter((e) =>
    e.start >= use.context[0] && e.end <= use.context[1] &&
    !(e.to === use.to && e.start === use.start && e.end === use.end) &&
    (e.kind === 'calls' || locationKinds.get(e.to) === 'field'))
    .slice(0, 8).map((e) => ({target: e.to, relation: e.kind === 'calls' ? 'call' : 'same_expression', start: e.start, end: e.end})) : []
  fieldUses.push(use)
}
process.stdout.write(JSON.stringify({compiler: ts.version, node: process.version, projects: [...configs.keys()],
  inferred_files: uncovered.length, diagnostics, diagnostic_count: diagnosticCount,
  documents: visited.size, mapped_definitions: definitions.size, skipped_occurrences: skipped,
  ambiguous_symbols: ambiguous, edges, field_uses: fieldUses,
  inputs: [...inputs].map(([path, hash]) => ({path, hash}))
    .concat([...absentDirectories].map((path) => ({path, hash: null, directory: true})))
    .concat([...directoryListings].map(([path, entries]) => ({path, hash: null, entries})))}))
