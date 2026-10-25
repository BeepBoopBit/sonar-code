// Sonar's embedded analyzer uses only the Go standard library. It never runs target code.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"go/ast"
	"go/importer"
	"go/parser"
	"go/token"
	"go/types"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
)

type location struct {
	ID        int64  `json:"id"`
	Path      string `json:"path"`
	Kind      string `json:"kind"`
	Start     int    `json:"start"`
	End       int    `json:"end"`
	NameStart int    `json:"name_start"`
	NameEnd   int    `json:"name_end"`
}
type request struct {
	Root        string            `json:"root"`
	Command     string            `json:"command"`
	Environment map[string]string `json:"environment"`
	Tags        []string          `json:"tags"`
	Files       []string          `json:"files"`
	Locations   []location        `json:"locations"`
}
type input struct {
	Path      string   `json:"path"`
	Hash      *string  `json:"hash"`
	Files     []string `json:"files,omitempty"`
	Directory bool     `json:"directory,omitempty"`
}
type edge struct {
	From  int64  `json:"from"`
	To    int64  `json:"to"`
	Kind  string `json:"kind"`
	Path  string `json:"path"`
	Line  int    `json:"line"`
	Start int    `json:"start"`
	End   int    `json:"end"`
	Text  string `json:"text"`
}
type field struct {
	edge
	Access  string  `json:"access"`
	Context *[2]int `json:"context"`
}
type export struct {
	Compiler        string   `json:"compiler"`
	Modules         []string `json:"modules"`
	Included        []string `json:"included_files"`
	Excluded        []string `json:"excluded_files"`
	Diagnostics     []string `json:"diagnostics"`
	DiagnosticCount int      `json:"diagnostic_count"`
	Definitions     int      `json:"mapped_definitions"`
	Ambiguous       int      `json:"ambiguous_symbols"`
	Skipped         int      `json:"skipped_occurrences"`
	Edges           []edge   `json:"edges"`
	Fields          []field  `json:"field_uses"`
	Inputs          []input  `json:"inputs"`
}
type module struct {
	Path, Dir, GoMod, GoVersion string
	Replace                     *module
}
type pkg struct {
	ImportPath, Dir, Name, Export                               string
	GoFiles, CgoFiles, CompiledGoFiles, IgnoredGoFiles, Imports []string
	ImportMap                                                   map[string]string
	Module                                                      *module
	Error                                                       *struct{ Err string }
}
type analyzer struct {
	req         request
	out         export
	locations   map[string][]location
	sources     map[string][]byte
	inputs      map[string]input
	edges       map[string]map[int64]edge
	fields      map[string]field
	seen        map[string]bool
	definitions map[int64]bool
}

func fail(err error) { fmt.Fprintln(os.Stderr, err); os.Exit(1) }
func main() {
	var req request
	if err := json.NewDecoder(os.Stdin).Decode(&req); err != nil {
		fail(err)
	}
	a := &analyzer{req: req, locations: map[string][]location{}, sources: map[string][]byte{}, inputs: map[string]input{}, edges: map[string]map[int64]edge{}, fields: map[string]field{}, seen: map[string]bool{}, definitions: map[int64]bool{}}
	a.out = export{Compiler: runtime.Version(), Modules: []string{}, Included: []string{}, Excluded: []string{}, Diagnostics: []string{}, Edges: []edge{}, Fields: []field{}, Inputs: []input{}}
	for _, l := range req.Locations {
		name := filepath.Clean(filepath.Join(req.Root, filepath.FromSlash(l.Path)))
		a.locations[name] = append(a.locations[name], l)
	}
	// Discover modules from indexed paths, including nested modules. go.work remains active.
	roots := map[string]bool{}
	for _, name := range req.Files {
		dir := filepath.Dir(filepath.Join(req.Root, filepath.FromSlash(name)))
		found := false
		for {
			for _, base := range []string{"go.mod", "go.sum", "go.work", "go.work.sum"} {
				a.fileInput(filepath.Join(dir, base))
			}
			if info, err := os.Stat(filepath.Join(dir, "go.mod")); err == nil && !info.IsDir() {
				roots[dir] = true
				found = true
				break
			}
			if dir == req.Root || filepath.Dir(dir) == dir {
				break
			}
			dir = filepath.Dir(dir)
		}
		if !found {
			roots[req.Root] = true
		}
	}
	for dir := req.Root; ; dir = filepath.Dir(dir) {
		a.fileInput(filepath.Join(dir, "go.work"))
		a.fileInput(filepath.Join(dir, "go.work.sum"))
		if filepath.Dir(dir) == dir {
			break
		}
	}
	if path := req.Environment["GOENV"]; path != "" && path != "off" {
		a.fileInput(path)
	}
	if path := req.Environment["GOWORK"]; path != "" && path != "off" {
		a.fileInput(path)
		a.fileInput(path + ".sum")
	}
	for _, dir := range sortedKeys(roots) {
		a.out.Modules = append(a.out.Modules, dir)
		if err := a.load(dir); err != nil {
			fail(err)
		}
	}
	for _, name := range req.Files {
		if a.seen[name] {
			a.out.Included = append(a.out.Included, name)
		} else {
			a.out.Excluded = append(a.out.Excluded, name)
		}
	}
	for _, key := range sortedKeys(a.edges) {
		targets := a.edges[key]
		if len(targets) != 1 {
			a.out.Ambiguous++
			continue
		}
		for _, e := range targets {
			a.out.Edges = append(a.out.Edges, e)
			if f, ok := a.fields[key]; ok && f.To == e.To {
				a.out.Fields = append(a.out.Fields, f)
			}
		}
	}
	a.out.Definitions = len(a.definitions)
	for _, path := range sortedKeys(a.inputs) {
		a.out.Inputs = append(a.out.Inputs, a.inputs[path])
	}
	if err := json.NewEncoder(os.Stdout).Encode(a.out); err != nil {
		fail(err)
	}
}

func sortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for key := range m {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}
func (a *analyzer) diagnostic(err error) {
	a.out.DiagnosticCount++
	if len(a.out.Diagnostics) < 30 {
		a.out.Diagnostics = append(a.out.Diagnostics, err.Error())
	}
}
func (a *analyzer) fileInput(path string) {
	path = filepath.Clean(path)
	if _, ok := a.inputs[path]; ok {
		return
	}
	data, err := os.ReadFile(path)
	var hash *string
	if err == nil {
		value := fmt.Sprintf("%x", sha256.Sum256(data))
		hash = &value
	} else if !os.IsNotExist(err) {
		fail(err)
	}
	a.inputs[path] = input{Path: path, Hash: hash}
}
func relevant(name string) bool {
	return strings.HasSuffix(name, ".go") || name == "go.mod" || name == "go.sum" || name == "go.work" || name == "go.work.sum"
}
func (a *analyzer) directoryInput(dir string) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		fail(err)
	}
	names := []string{}
	for _, e := range entries {
		if relevant(e.Name()) {
			names = append(names, e.Name())
			if !e.IsDir() {
				a.fileInput(filepath.Join(dir, e.Name()))
			}
		}
	}
	sort.Strings(names)
	a.inputs[dir] = input{Path: dir, Directory: true, Files: names}
}
func (a *analyzer) load(dir string) error {
	args := []string{"list", "-e", "-deps", "-json", "-compiled", "-export", "-test"}
	if len(a.req.Tags) > 0 {
		args = append(args, "-tags="+strings.Join(a.req.Tags, ","))
	}
	args = append(args, "./...")
	cmd := exec.Command(a.req.Command, args...)
	cmd.Dir = dir
	cmd.Env = os.Environ()
	for _, key := range sortedKeys(a.req.Environment) {
		cmd.Env = append(cmd.Env, key+"="+a.req.Environment[key])
	}
	flags := a.req.Environment["GOFLAGS"]
	_, moduleErr := os.Stat(filepath.Join(dir, "go.mod"))
	if moduleErr == nil || (a.req.Environment["GOMOD"] != "" && a.req.Environment["GOMOD"] != os.DevNull) {
		vendorRoot := dir
		if work := a.req.Environment["GOWORK"]; work != "" && work != "off" {
			vendorRoot = filepath.Dir(work)
		}
		vendorManifest := filepath.Join(vendorRoot, "vendor", "modules.txt")
		a.fileInput(vendorManifest)
		_, vendorErr := os.Stat(vendorManifest)
		if strings.Contains(flags, "-mod=vendor") || (vendorErr == nil && !strings.Contains(flags, "-mod=readonly")) {
			flags += " -mod=vendor"
		} else {
			flags += " -mod=readonly"
		}
	}
	cmd.Env = append(cmd.Env, "GOPROXY=off", "GOSUMDB=off", "GOTOOLCHAIN=local", "GOPACKAGESDRIVER=off", "GOFLAGS="+flags)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	data, runErr := cmd.Output()
	if runErr != nil && len(data) == 0 {
		return fmt.Errorf("Go package loading in %s: %w: %s", dir, runErr, stderr.String())
	}
	if runErr != nil {
		a.diagnostic(fmt.Errorf("Go package loading: %v: %s", runErr, stderr.String()))
	}
	l := &loader{a: a, pkgs: map[string]*pkg{}, checked: map[string]*types.Package{}, checking: map[string]bool{}, fset: token.NewFileSet()}
	decoder := json.NewDecoder(bytes.NewReader(data))
	for {
		var p pkg
		err := decoder.Decode(&p)
		if err == io.EOF {
			break
		}
		if err != nil {
			return err
		}
		copy := p
		l.pkgs[p.ImportPath] = &copy
		if p.Error != nil {
			a.diagnostic(fmt.Errorf("%s: %s", p.ImportPath, p.Error.Err))
		}
		if p.Dir != "" {
			a.directoryInput(p.Dir)
		}
		if p.Module != nil {
			a.moduleInputs(p.Module)
		}
		if p.Export != "" {
			a.fileInput(p.Export)
		}
	}
	if len(l.pkgs) == 0 {
		return fmt.Errorf("no Go packages loaded from %s", dir)
	}
	l.fallback = importer.ForCompiler(l.fset, "gc", func(path string) (io.ReadCloser, error) {
		p := l.pkgs[path]
		if p == nil || p.Export == "" {
			return nil, fmt.Errorf("no local compiler export for %s; prepare project dependencies before resolving", path)
		}
		return os.Open(p.Export)
	})
	for _, key := range sortedKeys(l.pkgs) {
		if l.indexed(l.pkgs[key]) {
			if _, err := l.check(key); err != nil {
				l.typeErrors = true
				a.diagnostic(err)
			}
		}
	}
	l.implementations()
	return nil
}
func (a *analyzer) moduleInputs(m *module) {
	if m.GoMod != "" {
		a.fileInput(m.GoMod)
		a.fileInput(filepath.Join(filepath.Dir(m.GoMod), "go.sum"))
		a.fileInput(strings.TrimSuffix(m.GoMod, ".mod") + ".sum")
	}
	if m.Replace != nil {
		a.moduleInputs(m.Replace)
	}
}

type loader struct {
	a          *analyzer
	pkgs       map[string]*pkg
	checked    map[string]*types.Package
	checking   map[string]bool
	fset       *token.FileSet
	fallback   types.Importer
	types      map[*types.TypeName][]types.Type
	typeErrors bool
}

func sourcePath(p *pkg, name string) string {
	if filepath.IsAbs(name) {
		return filepath.Clean(name)
	}
	return filepath.Join(p.Dir, name)
}
func (l *loader) indexed(p *pkg) bool {
	for _, name := range append(append([]string{}, p.GoFiles...), p.CgoFiles...) {
		if len(l.a.locations[sourcePath(p, name)]) > 0 {
			return true
		}
	}
	return false
}

type imports struct {
	l *loader
	p *pkg
}

func (i imports) Import(path string) (*types.Package, error) {
	if mapped, ok := i.p.ImportMap[path]; ok {
		path = mapped
	}
	if p := i.l.pkgs[path]; p != nil && i.l.indexed(p) {
		return i.l.check(path)
	}
	return i.l.fallback.Import(path)
}
func (l *loader) check(path string) (*types.Package, error) {
	if p := l.checked[path]; p != nil {
		return p, nil
	}
	if l.checking[path] {
		return nil, fmt.Errorf("import cycle involving %s", path)
	}
	l.checking[path] = true
	defer delete(l.checking, path)
	p := l.pkgs[path]
	names := p.CompiledGoFiles
	if len(names) == 0 {
		names = p.GoFiles
	}
	files := []*ast.File{}
	for _, name := range names {
		name = sourcePath(p, name)
		data, err := os.ReadFile(name)
		if err != nil {
			l.typeErrors = true
			l.a.diagnostic(err)
			continue
		}
		l.a.fileInput(name)
		file, err := parser.ParseFile(l.fset, name, data, parser.AllErrors)
		if err != nil {
			l.typeErrors = true
			l.a.diagnostic(err)
		}
		if file != nil {
			files = append(files, file)
		}
		if len(l.a.locations[name]) > 0 {
			l.a.sources[name] = data
		}
	}
	info := &types.Info{Defs: map[*ast.Ident]types.Object{}, Uses: map[*ast.Ident]types.Object{}, Types: map[ast.Expr]types.TypeAndValue{}, Selections: map[*ast.SelectorExpr]*types.Selection{}, Instances: map[*ast.Ident]types.Instance{}}
	config := types.Config{Importer: imports{l, p}, Sizes: types.SizesFor("gc", l.a.req.Environment["GOARCH"]), Error: func(err error) { l.typeErrors = true; l.a.diagnostic(err) }}
	if p.Module != nil && p.Module.GoVersion != "" {
		config.GoVersion = "go" + p.Module.GoVersion
	}
	checked, err := config.Check(path, l.fset, files, info)
	if checked == nil {
		return nil, err
	}
	l.checked[path] = checked
	for _, obj := range info.Defs {
		if name, ok := obj.(*types.TypeName); ok {
			l.implementationType(name, name.Type())
		}
	}
	for id, instance := range info.Instances {
		if name, ok := info.Uses[id].(*types.TypeName); ok {
			l.implementationType(name, instance.Type)
		}
	}
	for _, file := range files {
		name := l.fset.PositionFor(file.Pos(), false).Filename
		if len(l.a.locations[name]) > 0 {
			l.visit(file, name, info, checked)
		}
	}
	return checked, nil
}

func (l *loader) implementationType(name *types.TypeName, typ types.Type) {
	if typ == nil || l.mapped(name) == nil {
		return
	}
	if _, parameter := typ.(*types.TypeParam); parameter {
		return
	}
	// go/types predicates require instantiated generic types. This interface also
	// handles aliases on newer Go releases while keeping the helper Go 1.18 compatible.
	if generic, ok := typ.(interface {
		TypeParams() *types.TypeParamList
		TypeArgs() *types.TypeList
	}); ok && generic.TypeParams().Len() > generic.TypeArgs().Len() {
		return
	}
	if l.types == nil {
		l.types = map[*types.TypeName][]types.Type{}
	}
	for _, existing := range l.types[name] {
		if types.Identical(existing, typ) {
			return
		}
	}
	l.types[name] = append(l.types[name], typ)
}

// Implementation links are derived from complete method sets, so partial type
// checking is insufficient evidence. Calls and references still retain their
// independently resolved occurrences when a package load has errors.
func (l *loader) implementations() {
	if l.typeErrors {
		return
	}
	type contract struct {
		name *types.TypeName
		typ  *types.Interface
	}
	contracts := []contract{}
	for name, instances := range l.types {
		for _, typ := range instances {
			if iface, ok := typ.Underlying().(*types.Interface); ok {
				iface.Complete()
				// Empty interfaces and type-set constraints do not identify useful
				// concrete implementation bodies.
				if iface.IsMethodSet() && iface.NumMethods() > 0 {
					contracts = append(contracts, contract{name, iface})
				}
			}
		}
	}
	if len(contracts) == 0 {
		return
	}
	for name, instances := range l.types {
		for _, typ := range instances {
			if _, iface := typ.Underlying().(*types.Interface); iface {
				continue
			}
			for _, form := range []types.Type{typ, types.NewPointer(typ)} {
				methods := types.NewMethodSet(form)
				if methods.Len() == 0 {
					continue
				}
				for _, contract := range contracts {
					first := contract.typ.Method(0)
					if methods.Lookup(first.Pkg(), first.Name()) == nil || !types.Implements(form, contract.typ) {
						continue
					}
					l.implementation(name, contract.name)
					for i := 0; i < contract.typ.NumMethods(); i++ {
						required := contract.typ.Method(i)
						if method := methods.Lookup(required.Pkg(), required.Name()); method != nil {
							l.implementation(method.Obj(), required)
						}
					}
				}
			}
		}
	}
}

func (l *loader) implementation(from, to types.Object) {
	source, target := l.mapped(from), l.mapped(to)
	if source == nil || target == nil || source.ID == target.ID {
		return
	}
	name := filepath.Clean(filepath.Join(l.a.req.Root, filepath.FromSlash(source.Path)))
	data := l.a.sources[name]
	if source.NameStart < 0 || source.NameEnd > len(data) || source.NameStart >= source.NameEnd {
		return
	}
	e := edge{From: source.ID, To: target.ID, Kind: "implements", Path: source.Path, Line: l.fset.PositionFor(from.Pos(), false).Line, Start: source.NameStart, End: source.NameEnd, Text: string(data[source.NameStart:source.NameEnd])}
	// One declaration can implement many interfaces. These are independent
	// relationships, not ambiguous resolutions of a single source reference.
	key := fmt.Sprintf("implements:%d:%d", source.ID, target.ID)
	l.a.edges[key] = map[int64]edge{target.ID: e}
	l.a.definitions[source.ID] = true
	l.a.definitions[target.ID] = true
}

func (l *loader) mapped(obj types.Object) *location {
	if obj == nil || !obj.Pos().IsValid() {
		return nil
	}
	pos := l.fset.PositionFor(obj.Pos(), false)
	var found *location
	for _, loc := range l.a.locations[filepath.Clean(pos.Filename)] {
		if loc.NameStart == pos.Offset && loc.NameEnd == pos.Offset+len(obj.Name()) {
			if found != nil {
				return nil
			}
			copy := loc
			found = &copy
		}
	}
	return found
}
func (l *loader) span(node ast.Node) (int, int) {
	return l.fset.PositionFor(node.Pos(), false).Offset, l.fset.PositionFor(node.End(), false).Offset
}
func (l *loader) record(node ast.Node, obj types.Object, kind, name string) *edge {
	return l.recordRole(node, obj, kind, name, "name")
}
func (l *loader) recordRole(node ast.Node, obj types.Object, kind, name, role string) *edge {
	target := l.mapped(obj)
	if target == nil {
		l.a.out.Skipped++
		return nil
	}
	if kind == "calls" {
		if _, ok := obj.(*types.Func); !ok || target.Kind != "function" {
			return nil
		}
	}
	start, end := l.span(node)
	data := l.a.sources[name]
	if start < 0 || end > len(data) || start >= end {
		return nil
	}
	var owner *location
	for _, loc := range l.a.locations[name] {
		if loc.Start <= start && loc.End >= end && (owner == nil || loc.End-loc.Start < owner.End-owner.Start) {
			copy := loc
			owner = &copy
		}
	}
	if owner == nil {
		return nil
	}
	e := edge{From: owner.ID, To: target.ID, Kind: kind, Path: owner.Path, Line: l.fset.PositionFor(node.Pos(), false).Line, Start: start, End: end, Text: string(data[start:end])}
	key := edgeKey(e, role)
	if l.a.edges[key] == nil {
		l.a.edges[key] = map[int64]edge{}
	}
	l.a.edges[key][target.ID] = e
	l.a.definitions[target.ID] = true
	return &e
}

// An initializer value can also name a constant, variable, or function. Resolve
// ambiguity within each role so both source-supported relationships survive.
func edgeKey(e edge, role string) string {
	return fmt.Sprintf("%s:%d:%d:%s:%s", e.Path, e.Start, e.End, e.Kind, role)
}
func callable(expr ast.Expr, info *types.Info) (ast.Node, types.Object) {
	for {
		switch e := expr.(type) {
		case *ast.ParenExpr:
			expr = e.X
		case *ast.IndexExpr:
			expr = e.X
		case *ast.IndexListExpr:
			expr = e.X
		case *ast.Ident:
			return e, info.Uses[e]
		case *ast.SelectorExpr:
			return e.Sel, info.Uses[e.Sel]
		default:
			return nil, nil
		}
	}
}

// Follow only local function values with an initializer and no later writes or
// address-taking anywhere in the file, including nested closures. This avoids
// assuming a callback parameter, mutable global, field, or escaped variable has
// the same value as a similarly named function.
type functionValues struct {
	info         *types.Info
	initializers map[*types.Var]ast.Expr
	invalid      map[*types.Var]bool
	targets      map[*types.Var]*types.Func
	resolving    map[*types.Var]bool
}

func functionType(typ types.Type) bool {
	if typ == nil {
		return false
	}
	_, ok := typ.Underlying().(*types.Signature)
	return ok
}

func unparen(expr ast.Expr) ast.Expr {
	for {
		if paren, ok := expr.(*ast.ParenExpr); ok {
			expr = paren.X
		} else {
			return expr
		}
	}
}

func localFunctionValues(file *ast.File, info *types.Info, pkg *types.Package) *functionValues {
	values := &functionValues{
		info:         info,
		initializers: map[*types.Var]ast.Expr{},
		invalid:      map[*types.Var]bool{},
		targets:      map[*types.Var]*types.Func{},
		resolving:    map[*types.Var]bool{},
	}
	define := func(id *ast.Ident, expr ast.Expr) {
		variable, ok := info.Defs[id].(*types.Var)
		if !ok || variable.Parent() == pkg.Scope() || !functionType(variable.Type()) {
			return
		}
		if typ := info.TypeOf(expr); typ != nil && info.Types[expr].IsValue() && types.AssignableTo(typ, variable.Type()) {
			values.initializers[variable] = expr
		}
	}
	invalidate := func(expr ast.Expr) {
		if id, ok := unparen(expr).(*ast.Ident); ok {
			if variable, ok := info.Uses[id].(*types.Var); ok {
				values.invalid[variable] = true
			}
		}
	}
	ast.Inspect(file, func(node ast.Node) bool {
		switch n := node.(type) {
		case *ast.ValueSpec:
			if len(n.Names) == len(n.Values) {
				for i, id := range n.Names {
					define(id, n.Values[i])
				}
			}
		case *ast.AssignStmt:
			for i, left := range n.Lhs {
				invalidate(left)
				if n.Tok == token.DEFINE && len(n.Lhs) == len(n.Rhs) {
					if id, ok := left.(*ast.Ident); ok {
						define(id, n.Rhs[i])
					}
				}
			}
		case *ast.RangeStmt:
			invalidate(n.Key)
			invalidate(n.Value)
		case *ast.IncDecStmt:
			invalidate(n.X)
		case *ast.UnaryExpr:
			if n.Op == token.AND {
				invalidate(n.X)
			}
		}
		return true
	})
	return values
}

func (v *functionValues) target(expr ast.Expr) *types.Func {
	if expr == nil || !functionType(v.info.TypeOf(expr)) {
		return nil
	}
	// Conversions between function types preserve the function being called.
	if conversion, ok := unparen(expr).(*ast.CallExpr); ok {
		if len(conversion.Args) == 1 && v.info.Types[conversion.Fun].IsType() {
			return v.target(conversion.Args[0])
		}
		return nil
	}
	_, obj := callable(expr, v.info)
	switch obj := obj.(type) {
	case *types.Func:
		return obj
	case *types.Var:
		initializer := v.initializers[obj]
		if initializer == nil || v.invalid[obj] || v.resolving[obj] {
			return nil
		}
		if target, cached := v.targets[obj]; cached {
			return target
		}
		v.resolving[obj] = true
		target := v.target(initializer)
		delete(v.resolving, obj)
		v.targets[obj] = target
		return target
	default:
		return nil
	}
}

func (l *loader) visit(file *ast.File, name string, info *types.Info, pkg *types.Package) {
	for _, obj := range info.Defs {
		if loc := l.mapped(obj); loc != nil {
			l.a.definitions[loc.ID] = true
		}
	}
	path, _ := filepath.Rel(l.a.req.Root, name)
	l.a.seen[filepath.ToSlash(path)] = true
	parents := map[ast.Node]ast.Node{}
	stack := []ast.Node{}
	ast.Inspect(file, func(node ast.Node) bool {
		if node == nil {
			stack = stack[:len(stack)-1]
			return false
		}
		if len(stack) > 0 {
			parents[node] = stack[len(stack)-1]
		}
		stack = append(stack, node)
		return true
	})
	functions := localFunctionValues(file, info, pkg)
	ast.Inspect(file, func(node ast.Node) bool {
		switch n := node.(type) {
		case *ast.Ident:
			obj := info.Uses[n]
			if obj == nil {
				return true
			}
			if e := l.record(n, obj, "references", name); e != nil {
				if value, ok := obj.(*types.Var); ok && value.IsField() {
					f := field{edge: *e, Access: access(n, parents), Context: l.context(n, parents, *e)}
					l.a.fields[edgeKey(*e, "name")] = f
				}
			}
		case *ast.CallExpr:
			if target := functions.target(n.Fun); target != nil {
				site, _ := callable(n.Fun, info)
				if site == nil {
					site = n.Fun
				}
				l.record(site, target, "calls", name)
			}
		case *ast.CompositeLit:
			l.positional(n, name, info, pkg, parents)
		}
		return true
	})
}

func (l *loader) positional(lit *ast.CompositeLit, name string, info *types.Info, pkg *types.Package, parents map[ast.Node]ast.Node) {
	typ := info.TypeOf(lit)
	if typ == nil || len(lit.Elts) == 0 {
		return
	}
	underlying := typ.Underlying()
	// A nested literal can omit both its type and the & in []*Config{{...}}.
	if pointer, ok := underlying.(*types.Pointer); ok && lit.Type == nil {
		underlying = pointer.Elem().Underlying()
	}
	structure, ok := underlying.(*types.Struct)
	if !ok || len(lit.Elts) != structure.NumFields() {
		return
	}
	for i, element := range lit.Elts {
		if _, keyed := element.(*ast.KeyValueExpr); keyed {
			return
		}
		field := structure.Field(i)
		if !field.Exported() && field.Pkg() != pkg {
			return
		}
	}
	for i, element := range lit.Elts {
		target := structure.Field(i)
		if target.Name() == "_" {
			continue
		}
		if e := l.recordRole(element, target, "references", name, "initializer"); e != nil {
			l.a.fields[edgeKey(*e, "initializer")] = field{edge: *e, Access: "initialize", Context: l.context(element, parents, *e)}
		}
	}
}
func access(node *ast.Ident, parents map[ast.Node]ast.Node) string {
	var expr ast.Node = node
	if p, ok := parents[expr].(*ast.SelectorExpr); ok && p.Sel == node {
		expr = p
	}
	if p, ok := parents[node].(*ast.KeyValueExpr); ok && p.Key == node {
		return "initialize"
	}
	for {
		if p, ok := parents[expr].(*ast.ParenExpr); ok {
			expr = p
		} else {
			break
		}
	}
	switch p := parents[expr].(type) {
	case *ast.AssignStmt:
		for _, left := range p.Lhs {
			if left == expr {
				if p.Tok == token.ASSIGN || p.Tok == token.DEFINE {
					return "write"
				}
				return "read_write"
			}
		}
	case *ast.IncDecStmt:
		return "read_write"
	case *ast.RangeStmt:
		if p.Key == expr || p.Value == expr {
			return "write"
		}
	case *ast.UnaryExpr:
		if p.Op == token.AND {
			return "borrow"
		}
	case *ast.CallExpr:
		if p.Fun == expr {
			return "receiver"
		}
	case *ast.SelectorExpr:
		return "projection"
	case *ast.IndexExpr:
		if p.X == expr {
			return "projection"
		}
	}
	return "read"
}
func (l *loader) context(node ast.Node, parents map[ast.Node]ast.Node, e edge) *[2]int {
	for n := node; n != nil; n = parents[n] {
		_, stmt := n.(ast.Stmt)
		_, spec := n.(*ast.ValueSpec)
		if !stmt && !spec {
			continue
		}
		start, end := l.span(n)
		var owner location
		name := filepath.Join(l.a.req.Root, filepath.FromSlash(e.Path))
		for _, loc := range l.a.locations[name] {
			if loc.ID == e.From {
				owner = loc
				break
			}
		}
		data := l.a.sources[name]
		if start >= owner.Start && end <= owner.End && end-start <= 4096 && end <= len(data) && bytes.Count(data[start:end], []byte{'\n'}) < 16 {
			result := [2]int{start, end}
			return &result
		}
		return nil
	}
	return nil
}
